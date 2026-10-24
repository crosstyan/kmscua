//! The MCP tools as one-shot CLI verbs, for agents that run shell commands
//! but cannot register an MCP server.
//!
//! Each verb opens the display, does one thing, and exits: non-zero with a
//! message on failure. Coordinates are pixels of the LAST `xcua screenshot`,
//! mapped through `<state>/view.json` exactly as the MCP layer maps them
//! (see `view.rs`). While `xcua record` runs, every action is stamped into
//! its timeline, as MCP tool calls are.
//!
//! Every verb answers an [`Out`]: a line of text for people and models, and
//! a JSON value for `--json`.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Context, Result};
use kmscua_mcp::mcp::BatchAction;
use kmscua_mcp::Backend;
use kmscua_proto::{Button, CursorInfo, ImageFormat, Rect, Request, ScreenshotInfo};
use serde_json::{json, Value};

use crate::view::View;
use crate::xvfb::State;
use crate::{recd, XBackend};

/// What a verb answers.
pub struct Out {
    pub text: String,
    pub json: Value,
    /// Exit code on success paths that are not plain success (a wait that
    /// timed out): 0 otherwise.
    pub code: i32,
}

impl Out {
    pub fn ok(text: impl Into<String>, json: Value) -> Self {
        Self { text: text.into(), json, code: 0 }
    }
}

/// Exit code of a `wait-*` verb that timed out.
pub const TIMED_OUT: i32 = 3;

/// Default longest side of `zoom`, as for the MCP tools.
const ZOOM_MAX_SIDE: u32 = 1920;

/// One open display, with the view coordinates refer to.
pub struct Ctx {
    pub state: State,
    pub display: String,
    pub backend: XBackend,
    pub view: View,
}

impl Ctx {
    /// `warn_view`: say so when a saved view is set aside (not for
    /// `screenshot`, which replaces it anyway).
    pub fn open(state: State, display: &str, shm: bool, warn_view: bool) -> Result<Self> {
        let backend = XBackend::open(display, shm, &state.recordings())?;
        let screen = backend.screen_size()?;
        let (view, warning) = View::load(&state.dir, display, screen);
        if let (Some(w), true) = (warning, warn_view) {
            eprintln!("xcua: {w}");
        }
        Ok(Self { state, display: display.to_string(), backend, view })
    }

    fn call(&self, req: &Request) -> Result<kmscua_proto::Reply> {
        self.backend.call(req)
    }

    fn stamp(&self, what: &str) {
        recd::stamp(&self.state, what);
    }

    fn desktop(&self, x: i32, y: i32) -> (i32, i32) {
        self.view.to_desktop(x, y)
    }

    /// The pointer in screen pixels.
    fn pointer(&self) -> Result<(i32, i32)> {
        let c: CursorInfo = self.call(&Request::Cursor)?.data()?;
        Ok((c.x + c.hot_x, c.y + c.hot_y))
    }

    // ----- verbs -----

    /// Full screenshot to `out`; becomes the view later coordinates refer to.
    pub fn screenshot(&mut self, out: &Path, max_side: Option<u32>, jpeg: bool, cursor: bool) -> Result<Out> {
        let ext_jpeg = matches!(out.extension().and_then(|e| e.to_str()), Some("jpg" | "jpeg"));
        let format = if jpeg || ext_jpeg { ImageFormat::Jpeg } else { ImageFormat::Png };
        let reply = self.call(&Request::Screenshot { max_side, format, quality: None, cursor: Some(cursor), region: None })?;
        let info: ScreenshotInfo = reply.data()?;
        std::fs::write(out, &reply.payload).with_context(|| format!("write {}", out.display()))?;
        self.view = View::from_info(&self.display, &info, max_side);
        self.view.save(&self.state.dir)?;
        self.stamp("screenshot");
        Ok(Out::ok(
            format!(
                "{}: {}x{} of {}x{} on {} (scale {:.3}); coordinates now refer to this image",
                out.display(),
                info.width,
                info.height,
                info.source_width,
                info.source_height,
                self.display,
                info.scale
            ),
            json!({ "path": out, "view": self.view, "grab_ms": info.grab_ms, "encode_ms": info.encode_ms }),
        ))
    }

    pub fn click(&self, x: i32, y: i32, button: Button, count: u32, mods: &[String]) -> Result<Out> {
        let (dx, dy) = self.desktop(x, y);
        self.call(&Request::Click { x: dx, y: dy, button, count, modifiers: mods.to_vec() })?;
        let what = match (button, count) {
            (Button::Left, 2) => "double_click",
            (Button::Left, 3) => "triple_click",
            (Button::Left, _) => "left_click",
            (Button::Right, _) => "right_click",
            (Button::Middle, _) => "middle_click",
        };
        self.stamp(&format!("{what} [{x}, {y}]"));
        Ok(Out::ok("OK", json!({ "ok": true, "at": [x, y], "screen": [dx, dy] })))
    }

    pub fn move_to(&self, x: i32, y: i32) -> Result<Out> {
        let (dx, dy) = self.desktop(x, y);
        self.call(&Request::PointerMove { x: dx, y: dy })?;
        self.stamp(&format!("mouse_move [{x}, {y}]"));
        Ok(Out::ok("OK", json!({ "ok": true, "at": [x, y], "screen": [dx, dy] })))
    }

    pub fn button(&self, down: bool) -> Result<Out> {
        self.call(&Request::PointerButton { button: Button::Left, down })?;
        self.stamp(if down { "left_mouse_down" } else { "left_mouse_up" });
        Ok(Out::ok("OK", json!({ "ok": true })))
    }

    /// Drag from `from` (None = where the pointer is) to `to`.
    pub fn drag(&self, from: Option<(i32, i32)>, to: (i32, i32), mods: &[String]) -> Result<Out> {
        let (fx, fy) = match from {
            Some((x, y)) => self.desktop(x, y),
            None => self.pointer()?,
        };
        let (tx, ty) = self.desktop(to.0, to.1);
        self.call(&Request::Drag {
            from_x: fx,
            from_y: fy,
            to_x: tx,
            to_y: ty,
            button: Button::Left,
            steps: 16,
            modifiers: mods.to_vec(),
        })?;
        self.stamp(&format!("left_click_drag {from:?} -> [{}, {}]", to.0, to.1));
        Ok(Out::ok("OK", json!({ "ok": true })))
    }

    /// Wheel detents at a point (None = where the pointer is); `dy > 0` down.
    pub fn scroll(&self, at: Option<(i32, i32)>, dx: i32, dy: i32, mods: &[String]) -> Result<Out> {
        let (x, y) = match at {
            Some((x, y)) => self.desktop(x, y),
            None => self.pointer()?,
        };
        self.call(&Request::Scroll { x, y, dx, dy, modifiers: mods.to_vec() })?;
        self.stamp(&format!("scroll dx={dx} dy={dy}"));
        Ok(Out::ok("OK", json!({ "ok": true })))
    }

    pub fn type_text(&self, text: &str) -> Result<Out> {
        kmscua_mcp::type_text(&self.backend, text)?;
        let shown: String = text.chars().take(40).collect();
        self.stamp(&format!("type {shown:?}"));
        Ok(Out::ok("OK", json!({ "ok": true, "chars": text.chars().count() })))
    }

    pub fn key(&self, combos: &[String], repeat: u32) -> Result<Out> {
        if combos.is_empty() {
            bail!("key needs at least one combo, e.g. ctrl+l");
        }
        for _ in 0..repeat.clamp(1, 100) {
            for c in combos {
                self.call(&Request::KeyCombo { keys: vec![c.clone()] })?;
            }
        }
        self.stamp(&format!("key {}", combos.join(" ")));
        Ok(Out::ok("OK", json!({ "ok": true })))
    }

    pub fn hold(&self, keys: &str, seconds: f32) -> Result<Out> {
        self.call(&Request::KeyHold { keys: vec![keys.to_string()], seconds: seconds.clamp(0.0, 100.0) })?;
        self.stamp(&format!("hold_key {keys} {seconds}s"));
        Ok(Out::ok("OK", json!({ "ok": true })))
    }

    /// The pointer in the last screenshot's pixels.
    pub fn cursor(&self) -> Result<Out> {
        let (sx, sy) = self.pointer()?;
        let (x, y) = self.view.from_desktop(sx, sy);
        Ok(Out::ok(format!("X={x}, Y={y}"), json!({ "x": x, "y": y, "screen": [sx, sy] })))
    }

    /// A region of the last screenshot at up to native resolution.
    pub fn zoom(&self, region: [i32; 4], out: &Path, max_side: Option<u32>) -> Result<Out> {
        let [x0, y0, x1, y1] = region;
        let (w, h) = (x1 - x0, y1 - y0);
        if w <= 0 || h <= 0 {
            bail!("zoom region must be x0 y0 x1 y1 with x1 > x0 and y1 > y0");
        }
        let rect = self.view.rect_to_desktop(x0, y0, w, h);
        let reply = self.call(&Request::Screenshot {
            max_side: Some(max_side.unwrap_or(ZOOM_MAX_SIDE)),
            format: ImageFormat::Png,
            quality: None,
            cursor: Some(true),
            region: Some(rect),
        })?;
        let info: ScreenshotInfo = reply.data()?;
        std::fs::write(out, &reply.payload).with_context(|| format!("write {}", out.display()))?;
        self.stamp(&format!("zoom {region:?}"));
        Ok(Out::ok(
            format!(
                "{}: {}x{} of screen region {}x{}+{}+{}; coordinates still refer to the last screenshot",
                out.display(),
                info.width,
                info.height,
                rect.width,
                rect.height,
                rect.x,
                rect.y
            ),
            json!({ "path": out, "width": info.width, "height": info.height, "screen_region": [rect.x, rect.y, rect.width, rect.height] }),
        ))
    }

    fn region(&self, r: Option<[i32; 4]>) -> Option<Rect> {
        r.map(|[x, y, w, h]| self.view.rect_to_desktop(x, y, w, h))
    }

    /// A 320 px sample of the screen (or a region), as the MCP waits use.
    fn sample(&self, region: Option<Rect>) -> Result<Vec<u8>> {
        let reply = self.call(&Request::Screenshot {
            max_side: Some(320),
            format: ImageFormat::Png,
            quality: None,
            cursor: Some(false),
            region,
        })?;
        Ok(image::load_from_memory(&reply.payload)?.into_rgba8().into_raw())
    }

    /// Wait until the screen (or a region) stops changing for `quiet_ms`.
    pub fn wait_stable(&self, region: Option<[i32; 4]>, quiet_ms: u32, timeout_ms: u32, threshold: f32) -> Result<Out> {
        let region = self.region(region);
        let t0 = Instant::now();
        let mut prev = self.sample(region)?;
        let mut quiet_since = Instant::now();
        let ratio = loop {
            std::thread::sleep(Duration::from_millis(120));
            let cur = self.sample(region)?;
            let r = changed_ratio(&prev, &cur);
            if r > threshold {
                quiet_since = Instant::now();
            }
            prev = cur;
            if quiet_since.elapsed() >= Duration::from_millis(quiet_ms as u64) || t0.elapsed() >= Duration::from_millis(timeout_ms as u64) {
                break r;
            }
        };
        let stable = quiet_since.elapsed() >= Duration::from_millis(quiet_ms as u64);
        let secs = t0.elapsed().as_secs_f32();
        self.stamp(&format!("wait_for_stable -> {}", if stable { "settled" } else { "timeout" }));
        let text = if stable {
            format!("settled after {secs:.1}s")
        } else {
            format!("timed out after {secs:.1}s, still changing (last change ratio {ratio:.4})")
        };
        Ok(Out { text, json: json!({ "settled": stable, "seconds": secs, "ratio": ratio }), code: if stable { 0 } else { TIMED_OUT } })
    }

    /// Wait until the screen (or a region) differs from now by `threshold`.
    pub fn wait_change(&self, region: Option<[i32; 4]>, timeout_ms: u32, threshold: f32) -> Result<Out> {
        let region = self.region(region);
        let t0 = Instant::now();
        let base = self.sample(region)?;
        let (changed, ratio) = loop {
            std::thread::sleep(Duration::from_millis(120));
            let r = changed_ratio(&base, &self.sample(region)?);
            if r >= threshold {
                break (true, r);
            }
            if t0.elapsed() >= Duration::from_millis(timeout_ms as u64) {
                break (false, r);
            }
        };
        let secs = t0.elapsed().as_secs_f32();
        self.stamp(&format!("wait_for_change -> {}", if changed { "changed" } else { "timeout" }));
        let text = if changed {
            format!("changed after {secs:.1}s (change ratio {ratio:.4})")
        } else {
            format!("timed out after {secs:.1}s, no change (change ratio {ratio:.4})")
        };
        Ok(Out { text, json: json!({ "changed": changed, "seconds": secs, "ratio": ratio }), code: if changed { 0 } else { TIMED_OUT } })
    }

    pub fn focused(&self) -> Result<Out> {
        let text = self.backend.focused().ok_or_else(|| anyhow!("no focus information"))??;
        Ok(Out::ok(text.trim_end().to_string(), json!({ "focused": text })))
    }

    /// The MCP `computer_batch`: run in order, stop at the first failure.
    /// Screenshots (from `screenshot` and `wait`) and zooms are written to
    /// `out_dir` as `<index>-<action>.png`; a screenshot becomes the view
    /// for the actions after it.
    pub fn batch(&mut self, actions: &[BatchAction], out_dir: &Path) -> Result<Out> {
        std::fs::create_dir_all(out_dir).with_context(|| format!("create {}", out_dir.display()))?;
        let mut lines = Vec::new();
        let mut items = Vec::new();
        let mut failed = None;
        for (i, a) in actions.iter().enumerate() {
            if failed.is_some() {
                lines.push(format!("[{i}] {}: Not executed: an earlier computer action in this turn failed.", a.action));
                items.push(json!({ "action": a.action, "executed": false }));
                continue;
            }
            match self.batch_one(i, a, out_dir) {
                Ok(text) => {
                    lines.push(format!("[{i}] {}: {text}", a.action));
                    items.push(json!({ "action": a.action, "ok": true, "result": text }));
                }
                Err(e) => {
                    lines.push(format!("[{i}] {} failed: {e:#}", a.action));
                    items.push(json!({ "action": a.action, "ok": false, "error": format!("{e:#}") }));
                    failed = Some(i);
                }
            }
        }
        let text = lines.join("\n");
        if let Some(i) = failed {
            bail!("{text}\nbatch stopped at action {i}");
        }
        Ok(Out::ok(text, json!({ "ok": true, "actions": items })))
    }

    fn batch_one(&mut self, i: usize, a: &BatchAction, out_dir: &Path) -> Result<String> {
        let what = a.action.as_str();
        let at = |c: &Option<[i32; 2]>| c.ok_or_else(|| anyhow!("{what} needs `coordinate`"));
        let text = |t: &Option<String>| t.clone().ok_or_else(|| anyhow!("{what} needs `text`"));
        let mods = |t: &Option<String>| match t.as_deref().map(str::trim) {
            Some(m) if !m.is_empty() => vec![m.to_string()],
            _ => vec![],
        };
        let shot = |ctx: &mut Self| -> Result<String> {
            let p = out_dir.join(format!("{i:02}-{what}.png"));
            let max = ctx.view.max_side;
            ctx.screenshot(&p, max, false, true).map(|_| p.display().to_string())
        };
        Ok(match what {
            "screenshot" => shot(self)?,
            "zoom" => {
                let p = out_dir.join(format!("{i:02}-zoom.png"));
                self.zoom(a.region.ok_or_else(|| anyhow!("zoom needs `region`"))?, &p, None)?;
                p.display().to_string()
            }
            "cursor_position" => self.cursor()?.text,
            "mouse_move" => {
                let [x, y] = at(&a.coordinate)?;
                self.move_to(x, y)?.text
            }
            "left_click" | "right_click" | "middle_click" | "double_click" | "triple_click" => {
                let (button, count) = match what {
                    "right_click" => (Button::Right, 1),
                    "middle_click" => (Button::Middle, 1),
                    "double_click" => (Button::Left, 2),
                    "triple_click" => (Button::Left, 3),
                    _ => (Button::Left, 1),
                };
                let [x, y] = at(&a.coordinate)?;
                self.click(x, y, button, count, &mods(&a.text))?.text
            }
            "left_click_drag" => {
                let [x, y] = at(&a.coordinate)?;
                self.drag(a.start_coordinate.map(|[x, y]| (x, y)), (x, y), &mods(&a.text))?.text
            }
            "left_mouse_down" => self.button(true)?.text,
            "left_mouse_up" => self.button(false)?.text,
            "scroll" => {
                let dir = a.scroll_direction.clone().ok_or_else(|| anyhow!("scroll needs `scroll_direction`"))?;
                let n = a.scroll_amount.unwrap_or(3).min(100) as i32;
                let (dx, dy) = match dir.to_ascii_lowercase().as_str() {
                    "up" => (0, -n),
                    "down" => (0, n),
                    "left" => (-n, 0),
                    "right" => (n, 0),
                    other => bail!("scroll_direction must be up|down|left|right, got {other}"),
                };
                self.scroll(a.coordinate.map(|[x, y]| (x, y)), dx, dy, &mods(&a.text))?.text
            }
            "type" => self.type_text(&text(&a.text)?)?.text,
            "key" => self.key(&[text(&a.text)?], a.repeat.unwrap_or(1))?.text,
            "hold_key" => self.hold(&text(&a.text)?, a.duration.unwrap_or(1.0))?.text,
            "wait" => {
                std::thread::sleep(Duration::from_secs_f32(a.duration.unwrap_or(1.0).clamp(0.0, 100.0)));
                shot(self)?
            }
            other => bail!("unknown action {other}"),
        })
    }
}

/// Parse a batch: the MCP `{"actions": [...]}` or a bare array.
pub fn parse_batch(text: &str) -> Result<Vec<BatchAction>> {
    let v: Value = serde_json::from_str(text).context("batch is not JSON")?;
    let list = match v {
        Value::Object(mut o) => o.remove("actions").ok_or_else(|| anyhow!("batch object needs an `actions` array"))?,
        v @ Value::Array(_) => v,
        _ => bail!("batch must be {{\"actions\": [...]}} or an array of actions"),
    };
    serde_json::from_value(list).context("batch actions")
}

/// The MCP waits' measure: the fraction of pixels whose RGB moved by more
/// than 48 levels in sum.
fn changed_ratio(a: &[u8], b: &[u8]) -> f32 {
    if a.len() != b.len() || a.is_empty() {
        return 1.0;
    }
    let n = a.len() / 4;
    let diff = a
        .chunks_exact(4)
        .zip(b.chunks_exact(4))
        .filter(|(p, q)| (0..3).map(|i| (p[i] as i32 - q[i] as i32).abs()).sum::<i32>() > 48)
        .count();
    diff as f32 / n as f32
}

/// `text`, or stdin when it is `-`. Exactly one trailing newline is dropped
/// from stdin (what `echo` adds), so `echo hi | xcua type -` does not press
/// Return; `printf 'hi\n\n'` still sends one newline on purpose.
pub fn text_arg(text: &str) -> Result<String> {
    if text != "-" {
        return Ok(text.to_string());
    }
    let mut s = String::new();
    std::io::Read::read_to_string(&mut std::io::stdin(), &mut s).context("read stdin")?;
    Ok(strip_one_newline(s))
}

fn strip_one_newline(mut s: String) -> String {
    if s.ends_with('\n') {
        s.pop();
    }
    s
}

/// Held while an input verb runs, so verbs from two shells take turns
/// instead of interleaving their key and button events. Released on drop.
pub struct InputLock(#[allow(dead_code)] std::fs::File);

/// Wait (up to two minutes: a `hold` may run 100 s) for the state dir's
/// input lock.
pub fn input_lock(state: &State) -> Result<InputLock> {
    use std::os::fd::AsRawFd;
    std::fs::create_dir_all(&state.dir).with_context(|| format!("create {}", state.dir.display()))?;
    let path = state.dir.join("input.lock");
    let f = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(&path)
        .with_context(|| format!("open {}", path.display()))?;
    let deadline = Instant::now() + Duration::from_secs(120);
    loop {
        if unsafe { libc::flock(f.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0 {
            return Ok(InputLock(f));
        }
        let e = std::io::Error::last_os_error();
        match e.raw_os_error() {
            Some(libc::EINTR) => continue,
            Some(libc::EWOULDBLOCK) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(20)),
            Some(libc::EWOULDBLOCK) => bail!("another xcua has held the input for 2 minutes ({})", path.display()),
            // No flock on this filesystem: go without.
            _ => {
                log::info!("no input lock ({e}); verbs from several shells may interleave");
                return Ok(InputLock(f));
            }
        }
    }
}

/// Default directory for batch screenshots.
pub fn shots_dir(state: &State) -> PathBuf {
    state.dir.join("shots")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn batch_accepts_both_shapes() {
        let a = parse_batch(r#"{"actions": [{"action": "left_click", "coordinate": [1, 2]}, {"action": "type", "text": "hi"}]}"#).unwrap();
        assert_eq!(a.len(), 2);
        assert_eq!(a[0].coordinate, Some([1, 2]));
        let b = parse_batch(r#"[{"action": "key", "text": "ctrl+l", "repeat": 2}]"#).unwrap();
        assert_eq!((b[0].action.as_str(), b[0].repeat), ("key", Some(2)));
        assert!(parse_batch("{}").is_err() && parse_batch("3").is_err() && parse_batch("[{}]").is_err());
    }

    #[test]
    fn stdin_loses_exactly_one_newline() {
        assert_eq!(strip_one_newline("hello\n".into()), "hello");
        assert_eq!(strip_one_newline("hello\n\n".into()), "hello\n");
        assert_eq!(strip_one_newline("hello".into()), "hello");
    }

    #[test]
    fn changed_ratio_counts_moved_pixels() {
        let a = vec![0u8; 16];
        let mut b = a.clone();
        b[0] = 60;
        assert_eq!(changed_ratio(&a, &b), 0.25);
        b[0] = 40;
        assert_eq!(changed_ratio(&a, &b), 0.0);
        assert_eq!(changed_ratio(&a, &b[..8]), 1.0);
    }
}
