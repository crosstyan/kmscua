//! `kmscua_mcp::Backend` answered in-process from an X server: what cuad
//! does over its socket, without the socket.
//!
//! Capture and input hold one connection each, so a recording's grabs never
//! queue behind a key hold. Input requests run one at a time under a lock
//! (cuad's request thread); a failed one releases what it pressed before
//! the error goes back, and leaves down what an earlier request (a
//! `left_mouse_down`) holds. Every reply is shaped as cuad shapes it, so
//! the MCP layer's coordinate view and timeline work unchanged.
//!
//! Nothing here may take the server down with it: a lock whose holder
//! panicked is taken over rather than left poisoned, and a panic inside a
//! request comes back as an `Err` after everything is released, inside the
//! input lock so no other request sees keys left down.
//!
//! When the X server goes away (another `xcua mcp` stopped the Xvfb, it
//! crashed), a failed request finds the connection it used dead and
//! reconnects once, through the re-up hook when there is one so a new Xvfb
//! is started. Only reads (screenshot, cursor, displays, status, ping) are
//! retried. Input is never replayed: on a new server the windows it was
//! aimed at are gone, so the request answers "the display was restarted"
//! and nothing is sent. A server is told from a restarted one by a private
//! atom interned at connect ([`x11::ServerToken`]), which writes nothing to
//! the display; when a retried read is what discovers the restart, the
//! next input request carries the message instead.

use std::path::{Path, PathBuf};
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, TryLockError};
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Context, Result};
use kmscua_mcp::handedness::Handedness;
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

fn panic_message(payload: Box<dyn std::any::Any + Send>) -> String {
    payload
        .downcast_ref::<String>()
        .cloned()
        .or_else(|| payload.downcast_ref::<&str>().map(|s| s.to_string()))
        .unwrap_or_else(|| "panic without a message".into())
}

/// Starts or finds a display again after the X connection died; answers
/// the display to reconnect to.
pub type Reup = Box<dyn Fn() -> Result<String> + Send + Sync>;

pub struct XBackend {
    display: Mutex<String>,
    shm: bool,
    capture: Arc<Mutex<Capture>>,
    input: Mutex<Input>,
    /// Shared with every `Input`: raised at shutdown to cut holds short.
    cancel: Arc<AtomicBool>,
    reup: Option<Reup>,
    /// Identity of the server we are connected to.
    identity: Mutex<Option<x11::ServerToken>>,
    /// Set when a reconnect landed on a new server and no input request
    /// has been told yet.
    restarted: AtomicBool,
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
        let cancel = input.canceller();
        let identity = capture.server_token().map_err(|e| log::warn!("no server identity on {display}: {e:#}")).ok();
        Ok(Self {
            display: Mutex::new(display.to_string()),
            shm,
            capture: Arc::new(Mutex::new(capture)),
            input: Mutex::new(input),
            cancel,
            reup: None,
            identity: Mutex::new(identity),
            restarted: AtomicBool::new(false),
            rec: Mutex::new(RecSlot::default()),
            record_dir: record_dir.to_path_buf(),
            started: Instant::now(),
            served: AtomicU64::new(0),
        })
    }

    /// Use `reup` to get a display back when the X connection dies; without
    /// it the backend reconnects to the same display.
    pub fn with_reup(mut self, reup: Reup) -> Self {
        self.reup = Some(reup);
        self
    }

    /// The screen's size now, in pixels.
    pub fn screen_size(&self) -> Result<(u32, u32)> {
        lock(&self.capture).size()
    }

    pub fn display(&self) -> String {
        lock(&self.display).clone()
    }

    /// Release every key and button and unbind borrowed keycodes.
    pub fn release_all(&self) {
        lock(&self.input).release_all();
    }

    /// Shutdown: cut any hold, drag or typing short, wait at most `within`
    /// for the request running to give the input lock back, then release
    /// everything. Gives up (and says so in the log) rather than hang.
    pub fn shutdown(&self, within: Duration) {
        self.cancel.store(true, Ordering::Relaxed);
        let deadline = Instant::now() + within;
        loop {
            match self.input.try_lock() {
                Ok(mut i) => return i.release_all(),
                Err(TryLockError::Poisoned(p)) => return p.into_inner().release_all(),
                Err(TryLockError::WouldBlock) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(20)),
                Err(TryLockError::WouldBlock) => {
                    log::warn!("input still busy after {within:?} at shutdown; not releasing");
                    return;
                }
            }
        }
    }

    /// The capture behind a lock, for the recorder thread and the CLI.
    pub fn capture(&self) -> Arc<Mutex<Capture>> {
        self.capture.clone()
    }

    /// Run an input request under the lock. On error release what it
    /// pressed; on panic release everything, both before the lock is given
    /// back.
    fn input<T>(&self, f: impl FnOnce(&mut Input) -> Result<T>) -> Result<T> {
        let mut i = lock(&self.input);
        let before = i.held();
        match catch_unwind(AssertUnwindSafe(|| f(&mut i))) {
            Ok(Ok(v)) => Ok(v),
            Ok(Err(e)) => {
                i.release_since(&before);
                Err(e)
            }
            Err(p) => {
                i.release_all();
                bail!("internal: {}", panic_message(p))
            }
        }
    }

    /// Whether the connection `kind` uses is dead. Busy (another request
    /// holds it, a `hold_key` say) counts as alive: never wait on it.
    fn dead(&self, kind: Kind) -> bool {
        fn probe<T>(m: &Mutex<T>, alive: impl Fn(&T) -> bool) -> bool {
            match m.try_lock() {
                Ok(g) => !alive(&g),
                Err(TryLockError::Poisoned(p)) => !alive(&p.into_inner()),
                Err(TryLockError::WouldBlock) => false,
            }
        }
        match kind {
            Kind::Input => probe(&self.input, Input::alive),
            Kind::Read | Kind::Record => probe(&self.capture, Capture::alive),
        }
    }

    /// New connections to the display the re-up hook answers (or the same
    /// one), swapped in behind the locks so the recorder follows. Answers
    /// the display and whether it is the same server as before.
    fn reconnect(&self) -> Result<(String, bool)> {
        let d = match &self.reup {
            Some(f) => f().context("re-up")?,
            None => self.display(),
        };
        let cap = Capture::open(&d, self.shm)?;
        let mut inp = Input::open(&d)?;
        inp.set_canceller(self.cancel.clone());
        let same = lock(&self.identity).as_ref().is_some_and(|t| cap.same_server(t));
        let id = if same { lock(&self.identity).clone() } else { cap.server_token().ok() };
        *lock(&self.capture) = cap;
        *lock(&self.input) = inp;
        *lock(&self.display) = d.clone();
        *lock(&self.identity) = id;
        log::warn!("reconnected to X display {d} ({})", if same { "same server" } else { "a new server" });
        Ok((d, same))
    }

    /// `dispatch` with panics turned into errors.
    fn call_once(&self, req: &Request) -> Result<(serde_json::Value, Vec<u8>)> {
        match catch_unwind(AssertUnwindSafe(|| self.dispatch(req))) {
            Ok(r) => r,
            Err(p) => {
                self.release_all();
                bail!("internal: {}", panic_message(p))
            }
        }
    }

    fn status(&self) -> Result<Status> {
        let cap = lock(&self.capture);
        Ok(Status {
            version: env!("CARGO_PKG_VERSION").to_string(),
            build_id: env!("KMSCUA_BUILD_ID").to_string(),
            build: env!("KMSCUA_BUILD").to_string(),
            uptime_s: self.started.elapsed().as_secs(),
            gpu_driver: None,
            displays: cap.displays()?,
            desktop: cap.desktop()?,
            layout: cap.layout_source(),
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
                    ..Default::default()
                };
                (serde_json::to_value(self.record_start(opts)?)?, empty)
            }
            RecordStop => (serde_json::to_value(self.record_stop()?)?, empty),
            SetLayout { .. } => bail!(
                "SetLayout is not supported on X11: the X server owns the monitor layout (RandR) and xcua reads it from there; one Xvfb is one display"
            ),
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
        let displays: Vec<String> = lock(&self.capture)
            .displays()
            .map(|d| {
                d.iter()
                    .map(|m| format!("{} {}x{}+{}+{}", m.name, m.width, m.height, m.x, m.y))
                    .collect()
            })
            .unwrap_or_default();
        let (size, shm) = {
            let mut c = lock(&self.capture);
            // A grab tells whether SHM, when asked for, really works.
            let _ = c.grab(false, Some(kmscua_proto::Rect { x: 0, y: 0, width: 1, height: 1 }));
            (c.size().ok(), c.shm_active())
        };
        json!({
            "display": self.display(),
            "size": size.map(|(w, h)| format!("{w}x{h}")),
            "displays": displays,
            "build": env!("KMSCUA_BUILD"),
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

/// Which connection a request uses, and whether it may be retried.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Kind {
    /// Input: never retried after a reconnect.
    Input,
    /// Idempotent reads on the capture connection: retried once.
    Read,
    /// Recording: on the capture connection, not retried.
    Record,
}

impl Kind {
    fn of(req: &Request) -> Self {
        use Request::*;
        match req {
            Ping | Status | Displays | Screenshot { .. } | Cursor | SetLayout { .. } => Kind::Read,
            RecordStart { .. } | RecordStop | RecordStatus => Kind::Record,
            PointerMove { .. }
            | PointerButton { .. }
            | Click { .. }
            | Drag { .. }
            | Scroll { .. }
            | Key { .. }
            | KeyCombo { .. }
            | KeyHold { .. }
            | Type { .. }
            | Wake => Kind::Input,
        }
    }
}

fn restarted_message(display: &str) -> String {
    format!(
        "the display was restarted: {display} is a new X server with none of the windows you had; relaunch your apps (DISPLAY={display}), then take a screenshot. This request was not sent."
    )
}

impl Backend for XBackend {
    fn call(&self, req: &Request) -> Result<Reply> {
        self.served.fetch_add(1, Ordering::Relaxed);
        let kind = Kind::of(req);
        if kind == Kind::Input && self.restarted.swap(false, Ordering::Relaxed) {
            bail!("{}", restarted_message(&self.display()));
        }
        let (data, payload) = match self.call_once(req) {
            Ok(r) => r,
            Err(e) if self.dead(kind) => {
                let lost = self.display();
                let (d, same) = self
                    .reconnect()
                    .map_err(|re| anyhow!("X connection to {lost} was lost ({e:#}) and reconnecting failed: {re:#}"))?;
                match (kind, same) {
                    (Kind::Read, _) => {
                        if !same {
                            self.restarted.store(true, Ordering::Relaxed);
                        }
                        self.call_once(req)
                            .with_context(|| format!("X connection to {lost} was lost ({e:#}); retried on {d}"))?
                    }
                    (_, false) => bail!("{}", restarted_message(&d)),
                    (_, true) => bail!(
                        "X connection to {lost} was lost ({e:#}) and is back on the same server; this request was not repeated: check with a screenshot, then send it again"
                    ),
                }
            }
            Err(e) => return Err(e),
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

    fn handedness(&self) -> Option<Handedness> {
        Some(Handedness::normal("X server: XTEST buttons are not swapped by any desktop left-handed setting"))
    }

    fn doctor(&self, _buttons: Option<Handedness>) -> String {
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
