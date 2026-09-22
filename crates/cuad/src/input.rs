//! Virtual input through uinput: one absolute pointer and one keyboard,
//! created once at daemon start and kept for the daemon's lifetime.
//!
//! The pointer's `ABS_X`/`ABS_Y` range equals the scanout union rect, so
//! `ABS(x, y)` lands at scanout pixel `(x, y)`: the same space the screenshot
//! is in. Compositors map an absolute device across the whole desktop.
//!
//! Every public method is atomic: whatever it presses, it releases before it
//! returns, including on error. Another virtual device (a RustDesk session,
//! the real mouse) can interleave between calls, never inside one.

use std::thread::sleep;
use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};
use evdev::{
    uinput::VirtualDevice, AbsInfo, AbsoluteAxisCode, AttributeSet, EventType, InputEvent,
    KeyCode, PropType, RelativeAxisCode, UinputAbsSetup,
};
use kmscua_proto::{Button, Rect};

use crate::keymap;

pub const POINTER_NAME: &str = "kmscua absolute pointer";
pub const KEYBOARD_NAME: &str = "kmscua keyboard";

/// Device names carry the seat so a udev rule can route them:
/// `kmscua@<seat> absolute pointer`, `kmscua@<seat> keyboard`.
pub fn device_names(seat: Option<&str>) -> (String, String) {
    match seat {
        Some(s) => (format!("kmscua@{s} absolute pointer"), format!("kmscua@{s} keyboard")),
        None => (POINTER_NAME.to_string(), KEYBOARD_NAME.to_string()),
    }
}

const SYN: u16 = 0;
const KEY_MAX_CODE: u16 = 0x2ff;
/// KEY_MICMUTE, the last plain keyboard code before the BTN_* ranges.
const KEYBOARD_MAX_CODE: u16 = 248;

pub struct Input {
    pointer: VirtualDevice,
    keyboard: VirtualDevice,
    names: (String, String),
    desktop: Rect,
    /// Pause between press and release, and between clicks.
    tap_ms: u64,
}

impl Input {
    /// The uinput device names, as udev and the compositor see them.
    pub fn names(&self) -> (String, String) {
        self.names.clone()
    }

    pub fn create(desktop: Rect, seat: Option<&str>) -> Result<Self> {
        let (pointer_name, keyboard_name) = device_names(seat);
        let w = desktop.width.max(1) as i32;
        let h = desktop.height.max(1) as i32;
        let abs_x = UinputAbsSetup::new(
            AbsoluteAxisCode::ABS_X,
            AbsInfo::new(0, desktop.x, desktop.x + w - 1, 0, 0, 0),
        );
        let abs_y = UinputAbsSetup::new(
            AbsoluteAxisCode::ABS_Y,
            AbsInfo::new(0, desktop.y, desktop.y + h - 1, 0, 0, 0),
        );
        let buttons = AttributeSet::from_iter([
            KeyCode::BTN_LEFT,
            KeyCode::BTN_RIGHT,
            KeyCode::BTN_MIDDLE,
            KeyCode::BTN_SIDE,
            KeyCode::BTN_EXTRA,
        ]);
        let wheels =
            AttributeSet::from_iter([RelativeAxisCode::REL_WHEEL, RelativeAxisCode::REL_HWHEEL]);
        let props = AttributeSet::from_iter([PropType::DIRECT]);
        let pointer = VirtualDevice::builder()
            .context("uinput builder (is /dev/uinput present and writable?)")?
            .name(&pointer_name)
            .with_properties(&props)?
            .with_absolute_axis(&abs_x)?
            .with_absolute_axis(&abs_y)?
            .with_relative_axes(&wheels)?
            .with_keys(&buttons)?
            .build()
            .context("create uinput absolute pointer")?;

        // KEY_ESC..=KEY_MICMUTE: every keyboard key including media keys, and
        // nothing from the BTN_* ranges. Advertising joystick buttons makes
        // udev tag the device ID_INPUT_JOYSTICK and libinput then ignores it
        // as a keyboard (measured: Escape never arrived).
        let mut keys: AttributeSet<KeyCode> = AttributeSet::new();
        for code in 1..=KEYBOARD_MAX_CODE {
            keys.insert(KeyCode(code));
        }
        let keyboard = VirtualDevice::builder()
            .context("uinput builder")?
            .name(&keyboard_name)
            .with_keys(&keys)?
            .build()
            .context("create uinput keyboard")?;

        // udev and libinput need a moment to bind the new devices; RustDesk
        // measured ~400 ms. We pay it once, at start, never per request.
        sleep(Duration::from_millis(600));

        Ok(Self {
            pointer,
            keyboard,
            names: (pointer_name.clone(), keyboard_name.clone()),
            desktop,
            tap_ms: 30,
        })
    }

    pub fn desktop(&self) -> Rect {
        self.desktop
    }

    fn clamp(&self, x: i32, y: i32) -> (i32, i32) {
        let d = self.desktop;
        (
            x.clamp(d.x, d.x + d.width as i32 - 1),
            y.clamp(d.y, d.y + d.height as i32 - 1),
        )
    }

    pub fn move_to(&mut self, x: i32, y: i32) -> Result<()> {
        let (x, y) = self.clamp(x, y);
        self.pointer
            .emit(&[
                InputEvent::new_now(EventType::ABSOLUTE.0, AbsoluteAxisCode::ABS_X.0, x),
                InputEvent::new_now(EventType::ABSOLUTE.0, AbsoluteAxisCode::ABS_Y.0, y),
            ])
            .context("emit pointer motion")
    }

    pub fn button(&mut self, button: Button, down: bool) -> Result<()> {
        let code = button_code(button);
        self.pointer
            .emit(&[InputEvent::new_now(EventType::KEY.0, code, down as i32)])
            .context("emit pointer button")
    }

    /// Press the named modifier keys, run `f`, release them in reverse. The
    /// release happens even when `f` fails.
    pub fn with_modifiers<T>(
        &mut self,
        names: &[String],
        f: impl FnOnce(&mut Self) -> Result<T>,
    ) -> Result<T> {
        if names.is_empty() {
            return f(self);
        }
        let codes = keymap::parse_combo(names)
            .ok_or_else(|| anyhow!("unknown modifier in {names:?}"))?;
        let mut held = Vec::new();
        let mut result = Ok(());
        for &c in &codes {
            if let Err(e) = self.key(c, true) {
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
            let _ = self.key(c, false);
            sleep(Duration::from_millis(12));
        }
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
        let code = button_code(button);
        for i in 0..count.max(1) {
            if i > 0 {
                sleep(Duration::from_millis(60));
            }
            self.pointer
                .emit(&[InputEvent::new_now(EventType::KEY.0, code, 1)])
                .context("emit button press")?;
            sleep(Duration::from_millis(self.tap_ms));
            self.pointer
                .emit(&[InputEvent::new_now(EventType::KEY.0, code, 0)])
                .context("emit button release")?;
        }
        Ok(())
    }

    pub fn drag(
        &mut self,
        from: (i32, i32),
        to: (i32, i32),
        button: Button,
        steps: u32,
    ) -> Result<()> {
        let code = button_code(button);
        self.move_to(from.0, from.1)?;
        sleep(Duration::from_millis(self.tap_ms));
        self.pointer
            .emit(&[InputEvent::new_now(EventType::KEY.0, code, 1)])
            .context("emit drag press")?;
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
        let release = self
            .pointer
            .emit(&[InputEvent::new_now(EventType::KEY.0, code, 0)])
            .context("emit drag release");
        result.and(release)
    }

    /// Wheel detents; `dy > 0` scrolls down (content moves up).
    pub fn scroll(&mut self, x: i32, y: i32, dx: i32, dy: i32) -> Result<()> {
        self.move_to(x, y)?;
        sleep(Duration::from_millis(self.tap_ms));
        let (sx, sy) = (dx.signum(), dy.signum());
        for _ in 0..dx.unsigned_abs().min(50) {
            self.pointer
                .emit(&[InputEvent::new_now(
                    EventType::RELATIVE.0,
                    RelativeAxisCode::REL_HWHEEL.0,
                    sx,
                )])
                .context("emit hwheel")?;
            sleep(Duration::from_millis(15));
        }
        for _ in 0..dy.unsigned_abs().min(50) {
            // REL_WHEEL positive is "up" in evdev.
            self.pointer
                .emit(&[InputEvent::new_now(
                    EventType::RELATIVE.0,
                    RelativeAxisCode::REL_WHEEL.0,
                    -sy,
                )])
                .context("emit wheel")?;
            sleep(Duration::from_millis(15));
        }
        Ok(())
    }

    pub fn key(&mut self, code: u16, down: bool) -> Result<()> {
        if code == 0 || code > KEY_MAX_CODE {
            bail!("keycode {code} out of range");
        }
        self.keyboard
            .emit(&[InputEvent::new_now(EventType::KEY.0, code, down as i32)])
            .context("emit key")
    }

    /// Press the chord in order, release in reverse. Always releases.
    pub fn combo(&mut self, keys: &[String]) -> Result<()> {
        let codes = keymap::parse_combo(keys)
            .ok_or_else(|| anyhow!("unknown key in combo {keys:?}"))?;
        let mut pressed = Vec::with_capacity(codes.len());
        let mut result = Ok(());
        for &c in &codes {
            if let Err(e) = self.key(c, true) {
                result = Err(e);
                break;
            }
            pressed.push(c);
            sleep(Duration::from_millis(12));
        }
        sleep(Duration::from_millis(self.tap_ms));
        for &c in pressed.iter().rev() {
            if let Err(e) = self.key(c, false) {
                if result.is_ok() {
                    result = Err(e);
                }
            }
            sleep(Duration::from_millis(12));
        }
        result
    }

    /// ASCII only. Returns the index of the first character it could not type.
    pub fn type_text(&mut self, text: &str, delay_ms: u32) -> Result<()> {
        let delay = Duration::from_millis(delay_ms.max(4) as u64);
        for (i, ch) in text.chars().enumerate() {
            let (code, shift) = keymap::char_to_key(ch)
                .ok_or_else(|| anyhow!("cannot type {ch:?} at index {i}: not on the US keymap; paste it via the clipboard"))?;
            let shift_code = KeyCode::KEY_LEFTSHIFT.0;
            if shift {
                self.key(shift_code, true)?;
            }
            let r = self
                .key(code, true)
                .and_then(|_| {
                    sleep(delay);
                    self.key(code, false)
                });
            if shift {
                let _ = self.key(shift_code, false);
            }
            r?;
            sleep(delay);
        }
        Ok(())
    }

    /// One-pixel nudge and back, to wake a blanked output. Harmless otherwise.
    pub fn wake(&mut self) -> Result<()> {
        self.pointer
            .emit(&[InputEvent::new_now(
                EventType::RELATIVE.0,
                RelativeAxisCode::REL_WHEEL.0,
                0,
            )])
            .ok();
        let d = self.desktop;
        let cx = d.x + d.width as i32 / 2;
        let cy = d.y + d.height as i32 / 2;
        self.move_to(cx + 1, cy)?;
        sleep(Duration::from_millis(20));
        self.move_to(cx, cy)
    }

    /// Release everything we could possibly be holding. Used on shutdown.
    pub fn release_all(&mut self) {
        for b in [Button::Left, Button::Right, Button::Middle] {
            let _ = self.button(b, false);
        }
        for c in [
            KeyCode::KEY_LEFTSHIFT,
            KeyCode::KEY_RIGHTSHIFT,
            KeyCode::KEY_LEFTCTRL,
            KeyCode::KEY_RIGHTCTRL,
            KeyCode::KEY_LEFTALT,
            KeyCode::KEY_RIGHTALT,
            KeyCode::KEY_LEFTMETA,
            KeyCode::KEY_RIGHTMETA,
        ] {
            let _ = self.key(c.0, false);
        }
        let _ = self
            .keyboard
            .emit(&[InputEvent::new_now(EventType::SYNCHRONIZATION.0, SYN, 0)]);
    }
}

fn button_code(b: Button) -> u16 {
    match b {
        Button::Left => KeyCode::BTN_LEFT.0,
        Button::Right => KeyCode::BTN_RIGHT.0,
        Button::Middle => KeyCode::BTN_MIDDLE.0,
    }
}
