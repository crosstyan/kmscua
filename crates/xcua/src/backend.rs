//! `kmscua_mcp::Backend` answered in-process from an X server: what cuad
//! does over its socket, without the socket.
//!
//! Capture and input hold one connection each, so a recording's grabs never
//! queue behind a key hold. Input requests run one at a time under a lock
//! (cuad's request thread); a failed one releases everything before the
//! error goes back. Every reply is shaped as cuad shapes it, so the MCP
//! layer's coordinate view and timeline work unchanged.
//!
//! Nothing here may take the server down with it: a lock whose holder
//! panicked is taken over rather than left poisoned, and a panic inside a
//! request comes back as an `Err` after everything is released.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Instant;

use anyhow::{anyhow, bail, Result};
use kmscua_mcp::Backend;
use kmscua_proto::{Header, RecordInfo, Reply, Request, Status};
use serde_json::json;

use crate::input::Input;
use crate::pixels;
use crate::record::{Grab, RecordOpts, Recorder};
use crate::x11::{self, Capture};

/// `RecordInfo::codec` and `Status::record_codec` for stills recordings.
pub const RECORD_CODEC: &str = "jpeg-stills";

/// Lock, taking over from a holder that panicked: the data behind these
/// locks (a connection, a recorder slot) stays usable, and one failed
/// request must not make every later one fail with "poisoned".
fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

pub struct XBackend {
    display: String,
    capture: Arc<Mutex<Capture>>,
    input: Mutex<Input>,
    rec: Mutex<RecSlot>,
    record_dir: PathBuf,
    started: Instant,
    served: AtomicU64,
}

#[derive(Default)]
struct RecSlot {
    recorder: Option<Recorder>,
    last: Option<RecordInfo>,
}

impl XBackend {
    /// Connect capture and input to `display`. `shm` opts in to MIT-SHM
    /// capture; recordings go to `record_dir`.
    pub fn open(display: &str, shm: bool, record_dir: &Path) -> Result<Self> {
        let capture = Capture::open(display, shm)?;
        let input = Input::open(display)?;
        Ok(Self {
            display: display.to_string(),
            capture: Arc::new(Mutex::new(capture)),
            input: Mutex::new(input),
            rec: Mutex::new(RecSlot::default()),
            record_dir: record_dir.to_path_buf(),
            started: Instant::now(),
            served: AtomicU64::new(0),
        })
    }

    pub fn display(&self) -> &str {
        &self.display
    }

    /// Release every key and button and unbind borrowed keycodes.
    pub fn release_all(&self) {
        lock(&self.input).release_all();
    }

    /// The capture behind a lock, for the recorder thread and the CLI.
    pub fn capture(&self) -> Arc<Mutex<Capture>> {
        self.capture.clone()
    }

    fn input<T>(&self, f: impl FnOnce(&mut Input) -> Result<T>) -> Result<T> {
        let mut i = lock(&self.input);
        let r = f(&mut i);
        if r.is_err() {
            i.release_all();
        }
        r
    }

    fn status(&self) -> Result<Status> {
        let cap = lock(&self.capture);
        Ok(Status {
            version: env!("CARGO_PKG_VERSION").to_string(),
            uptime_s: self.started.elapsed().as_secs(),
            gpu_driver: None,
            displays: cap.displays()?,
            desktop: cap.desktop()?,
            pointer_device: "xtest".into(),
            keyboard_device: "xtest".into(),
            cursor_supported: cap.cursor_supported(),
            requests_served: self.served.load(Ordering::Relaxed),
            record_codec: Some(RECORD_CODEC.into()),
            record_dir: Some(self.record_dir.display().to_string()),
        })
    }

    fn idle_record_info() -> RecordInfo {
        RecordInfo {
            recording: false,
            path: None,
            codec: Some(RECORD_CODEC.into()),
            width: 0,
            height: 0,
            fps: 0,
            frames: 0,
            dropped: 0,
            seconds: 0.0,
            finalized: None,
            error: None,
        }
    }

    fn record_start(&self, opts: RecordOpts) -> Result<RecordInfo> {
        let mut slot = lock(&self.rec);
        if let Some(r) = &slot.recorder {
            let info = r.info();
            if info.recording {
                bail!("already recording {}", info.path.as_deref().unwrap_or("?"));
            }
        }
        std::fs::create_dir_all(&self.record_dir)
            .map_err(|e| anyhow!("create {}: {e}", self.record_dir.display()))?;
        let cap = self.capture.clone();
        let grab: Grab = Box::new(move |cursor| {
            Ok(lock(&cap).grab(cursor, None)?.rgba)
        });
        let rec = Recorder::start(&self.record_dir, opts, grab)?;
        let info = rec.info();
        slot.recorder = Some(rec);
        slot.last = None;
        Ok(info)
    }

    fn record_stop(&self) -> Result<RecordInfo> {
        let mut slot = lock(&self.rec);
        let mut r = slot.recorder.take().ok_or_else(|| anyhow!("not recording"))?;
        let info = r.stop();
        slot.last = Some(info.clone());
        Ok(info)
    }

    fn record_status(&self) -> RecordInfo {
        let slot = lock(&self.rec);
        match (&slot.recorder, &slot.last) {
            (Some(r), _) => r.info(),
            (None, Some(last)) => last.clone(),
            (None, None) => Self::idle_record_info(),
        }
    }

    fn dispatch(&self, req: &Request) -> Result<(serde_json::Value, Vec<u8>)> {
        use Request::*;
        let empty = Vec::new();
        let ok = || json!({});
        Ok(match req {
            Ping => (json!({"pong": true}), empty),
            Status => (serde_json::to_value(self.status()?)?, empty),
            Displays => (serde_json::to_value(lock(&self.capture).displays()?)?, empty),
            Screenshot { max_side, format, quality, cursor, region } => {
                let shot = lock(&self.capture).grab(cursor.unwrap_or(true), *region)?;
                let (info, bytes) = pixels::finish(shot, *max_side, *format, *quality)?;
                (serde_json::to_value(info)?, bytes)
            }
            Cursor => (serde_json::to_value(lock(&self.capture).cursor()?)?, empty),
            PointerMove { x, y } => {
                self.input(|i| i.move_to(*x, *y).and_then(|_| i.sync()))?;
                (ok(), empty)
            }
            PointerButton { button, down } => {
                self.input(|i| i.button(*button, *down))?;
                (ok(), empty)
            }
            Click { x, y, button, count, modifiers } => {
                self.input(|i| i.with_modifiers(modifiers, |i| i.click(*x, *y, *button, *count)))?;
                (ok(), empty)
            }
            Drag { from_x, from_y, to_x, to_y, button, steps, modifiers } => {
                self.input(|i| {
                    i.with_modifiers(modifiers, |i| i.drag((*from_x, *from_y), (*to_x, *to_y), *button, *steps))
                })?;
                (ok(), empty)
            }
            Scroll { x, y, dx, dy, modifiers } => {
                self.input(|i| i.with_modifiers(modifiers, |i| i.scroll(*x, *y, *dx, *dy)))?;
                (ok(), empty)
            }
            Key { code, down } => {
                self.input(|i| i.key(*code, *down))?;
                (ok(), empty)
            }
            KeyCombo { keys } => {
                self.input(|i| i.combo(keys))?;
                (ok(), empty)
            }
            KeyHold { keys, seconds } => {
                self.input(|i| i.hold(keys, *seconds))?;
                (ok(), empty)
            }
            Type { text, delay_ms } => {
                self.input(|i| i.type_text(text, delay_ms.unwrap_or(12)))?;
                (json!({"typed": text.chars().count()}), empty)
            }
            Wake => {
                self.input(|i| i.wake())?;
                (ok(), empty)
            }
            RecordStart { name, fps, max_side, bitrate_kbps: _, max_seconds, cursor } => {
                let opts = RecordOpts {
                    name: name.clone(),
                    fps: *fps,
                    max_side: *max_side,
                    max_seconds: *max_seconds,
                    cursor: *cursor,
                };
                (serde_json::to_value(self.record_start(opts)?)?, empty)
            }
            RecordStop => (serde_json::to_value(self.record_stop()?)?, empty),
            RecordStatus => (serde_json::to_value(self.record_status())?, empty),
        })
    }

    /// The `doctor` report: display, extensions, capture path, focus,
    /// recording dir, and what to do about anything missing.
    pub fn doctor_report(&self) -> serde_json::Value {
        let mut notes = Vec::new();
        let (extensions, focus, wm, spare) = {
            let i = lock(&self.input);
            let ext: serde_json::Map<String, serde_json::Value> =
                x11::extensions(i.conn()).into_iter().map(|(n, p)| (n.to_string(), json!(p))).collect();
            let focus = x11::focused_text(i.conn(), i.root()).unwrap_or_else(|e| format!("error: {e:#}"));
            let spare = i.keymap().map(|m| m.spare().len()).unwrap_or(0);
            (json!(ext), focus, x11::has_wm(i.conn(), i.root()), spare)
        };
        if extensions.get("XFIXES") != Some(&json!(true)) {
            notes.push("no XFIXES: screenshots will not show the pointer and cursor_position has no hotspot");
        }
        if extensions.get("GLX") != Some(&json!(true)) {
            notes.push("no GLX: OpenGL apps (browsers with GPU paths, games) will fail or fall back to software");
        }
        if spare == 0 {
            notes.push("no free keycode: characters outside the keyboard layout cannot be typed");
        }
        let (size, shm) = {
            let mut c = lock(&self.capture);
            // A grab tells whether SHM, when asked for, really works.
            let _ = c.grab(false, Some(kmscua_proto::Rect { x: 0, y: 0, width: 1, height: 1 }));
            (c.size().ok(), c.shm_active())
        };
        json!({
            "display": self.display,
            "size": size.map(|(w, h)| format!("{w}x{h}")),
            "extensions": extensions,
            "capture": if shm { "MIT-SHM" } else { "GetImage" },
            "window_manager": wm,
            "focus_on_click": !wm,
            "spare_keycodes": spare,
            "focus": focus,
            "record_dir": self.record_dir.display().to_string(),
            "record_status": self.record_status(),
            "notes": notes,
        })
    }
}

impl Backend for XBackend {
    fn call(&self, req: &Request) -> Result<Reply> {
        self.served.fetch_add(1, Ordering::Relaxed);
        // Last line: a panic anywhere below becomes this request's error,
        // with nothing left pressed, instead of a dead task and a poisoned
        // server. The locks are reacquired with `lock`, so the state stays
        // reachable.
        let (data, payload) = match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| self.dispatch(req))) {
            Ok(r) => r?,
            Err(payload) => {
                let msg = payload
                    .downcast_ref::<String>()
                    .cloned()
                    .or_else(|| payload.downcast_ref::<&str>().map(|s| s.to_string()))
                    .unwrap_or_else(|| "panic without a message".into());
                self.release_all();
                bail!("internal: {msg}");
            }
        };
        Ok(Reply {
            header: Header { ok: true, error: None, data, bytes: payload.len() as u64 },
            payload,
        })
    }

    fn types_unicode(&self) -> bool {
        true
    }

    fn focused(&self) -> Option<Result<String>> {
        let i = lock(&self.input);
        Some(x11::focused_text(i.conn(), i.root()))
    }

    fn doctor(&self) -> String {
        serde_json::to_string_pretty(&self.doctor_report()).unwrap_or_default()
    }
}

impl Drop for XBackend {
    fn drop(&mut self) {
        if let Some(mut r) = lock(&self.rec).recorder.take() {
            if r.info().recording {
                let _ = r.stop();
            }
        }
    }
}
