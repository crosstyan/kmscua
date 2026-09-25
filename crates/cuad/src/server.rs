//! Unix-socket request loop plus a capture worker thread.
//!
//! Input runs on the request thread (one request at a time, so actions never
//! interleave). Capture and recording run on a worker that owns the DRM
//! context: it answers screenshot requests over a channel and, while a
//! recording is active, grabs a frame on every tick between requests.

use std::fs;
use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, RecvTimeoutError, Sender};
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Context, Result};
use kmscua_proto::{
    read_json_frame, write_frame, Header, ImageFormat, RecordInfo, Rect, Request, Status,
};
use serde_json::json;

use crate::capture::Capturer;
use crate::input::Input;
use crate::recorder::{self, Codec, RecordOpts, Recorder};

/// None = detection still running; Some(None) = recording unavailable.
pub type CodecSlot = std::sync::Arc<std::sync::Mutex<Option<Option<Codec>>>>;

pub struct RecordConfig {
    pub codec: CodecSlot,
    pub dir: PathBuf,
    /// uid to chown finished recordings to (the socket owner), if any.
    pub owner_uid: Option<u32>,
    /// Record through the Jetson zero-copy backend (probed at startup);
    /// the GStreamer codec, if any, is the fallback.
    pub zero_copy: bool,
}

enum CaptureCmd {
    Screenshot {
        cursor: bool,
        region: Option<Rect>,
        max_side: Option<u32>,
        format: ImageFormat,
        quality: Option<u8>,
        reply: Sender<Result<(serde_json::Value, Vec<u8>)>>,
    },
    Cursor(Sender<Result<serde_json::Value>>),
    Displays(Sender<Result<serde_json::Value>>),
    Info(Sender<(Option<String>, bool, Option<String>)>),
    RecordStart {
        name: Option<String>,
        fps: u32,
        max_side: u32,
        bitrate_kbps: u32,
        max_seconds: u32,
        cursor: bool,
        reply: Sender<Result<RecordInfo>>,
    },
    RecordStop(Sender<Result<RecordInfo>>),
    RecordStatus(Sender<RecordInfo>),
}

struct CaptureWorker {
    capture: Capturer,
    rec: RecordConfig,
    recorder: Option<Recorder>,
    last_info: Option<RecordInfo>,
}

impl CaptureWorker {
    fn run(mut self, rx: mpsc::Receiver<CaptureCmd>) {
        loop {
            let timeout = match &self.recorder {
                Some(r) if r.is_running() => r
                    .next_tick()
                    .saturating_duration_since(Instant::now()),
                _ => Duration::from_secs(3600),
            };
            match rx.recv_timeout(timeout) {
                Ok(cmd) => self.handle(cmd),
                Err(RecvTimeoutError::Timeout) => self.tick(),
                Err(RecvTimeoutError::Disconnected) => {
                    if let Some(mut r) = self.recorder.take() {
                        let _ = r.stop();
                    }
                    return;
                }
            }
        }
    }

    fn tick(&mut self) {
        let Some(rec) = self.recorder.as_mut() else { return };
        if !rec.is_running() {
            return;
        }
        if rec.over_time() {
            log::info!("recorder: max_seconds reached, stopping");
            self.finish_recording();
            return;
        }
        let factor = rec.factor();
        let want_cursor = rec.wants_cursor();
        if rec.zero_copy() {
            let shot = self
                .capture
                .grab_scanout(want_cursor)
                .map_err(|e| log::debug!("recorder: grab failed, repeating frame: {e:#}"))
                .ok();
            if let Err(e) = rec.push_scanout(shot.as_ref()) {
                log::warn!("recorder: {e:#}; stopping");
                self.finish_recording();
            }
            return;
        }
        let frame = match self.capture.grab(want_cursor, None) {
            Ok(shot) => {
                let mut img = recorder::downscale(&shot.rgba, factor);
                let (w, h) = (rec_dim(rec).0, rec_dim(rec).1);
                if img.dimensions() != (w, h) {
                    img = image::imageops::crop_imm(&img, 0, 0, w, h).to_image();
                }
                Some(img)
            }
            Err(e) => {
                log::debug!("recorder: grab failed, repeating frame: {e:#}");
                None
            }
        };
        if let Err(e) = rec.push(frame.as_ref()) {
            log::warn!("recorder: {e:#}; stopping");
            self.finish_recording();
        }
    }

    fn finish_recording(&mut self) {
        if let Some(mut r) = self.recorder.take() {
            let info = r.stop();
            if let Some(uid) = self.rec.owner_uid {
                chown_path(r.path(), uid);
            }
            log::info!(
                "recorder: stopped {} frames={} dropped={} finalized={:?} {}",
                info.path.as_deref().unwrap_or("?"),
                info.frames,
                info.dropped,
                info.finalized,
                info.error.as_deref().unwrap_or("")
            );
            self.last_info = Some(info);
        }
    }

    fn handle(&mut self, cmd: CaptureCmd) {
        match cmd {
            CaptureCmd::Screenshot {
                cursor,
                region,
                max_side,
                format,
                quality,
                reply,
            } => {
                let r = self
                    .capture
                    .screenshot(cursor, region, max_side, format, quality)
                    .and_then(|(info, bytes)| Ok((serde_json::to_value(info)?, bytes)));
                let _ = reply.send(r);
            }
            CaptureCmd::Cursor(reply) => {
                let _ = reply.send(self.capture.cursor().and_then(|c| Ok(serde_json::to_value(c)?)));
            }
            CaptureCmd::Displays(reply) => {
                let _ = reply.send(self.capture.displays().and_then(|d| Ok(serde_json::to_value(d)?)));
            }
            CaptureCmd::Info(reply) => {
                let codec = match *self.rec.codec.lock().unwrap() {
                    _ if self.rec.zero_copy => Some(recorder::ZERO_COPY.to_string()),
                    None => Some("detecting".to_string()),
                    Some(c) => c.map(|c| c.name().to_string()),
                };
                let _ = reply.send((self.capture.gpu_driver(), self.capture.cursor_supported(), codec));
            }
            CaptureCmd::RecordStart {
                name,
                fps,
                max_side,
                bitrate_kbps,
                max_seconds,
                cursor,
                reply,
            } => {
                let _ = reply.send(self.start_recording(name, fps, max_side, bitrate_kbps, max_seconds, cursor));
            }
            CaptureCmd::RecordStop(reply) => {
                if self.recorder.is_some() {
                    self.finish_recording();
                    let _ = reply.send(self.last_info.clone().ok_or_else(|| anyhow!("no recording")));
                } else {
                    let _ = reply.send(Err(anyhow!("not recording")));
                }
            }
            CaptureCmd::RecordStatus(reply) => {
                let info = match &self.recorder {
                    Some(r) => r.info(),
                    None => self.last_info.clone().unwrap_or(RecordInfo {
                        recording: false,
                        path: None,
                        codec: match self.rec.zero_copy {
                            true => Some(recorder::ZERO_COPY.to_string()),
                            false => self.codec().map(|c| c.name().to_string()),
                        },
                        width: 0,
                        height: 0,
                        fps: 0,
                        frames: 0,
                        dropped: 0,
                        seconds: 0.0,
                        finalized: None,
                        error: None,
                    }),
                };
                let _ = reply.send(info);
            }
        }
    }

    fn start_recording(
        &mut self,
        name: Option<String>,
        fps: u32,
        max_side: u32,
        bitrate_kbps: u32,
        max_seconds: u32,
        cursor: bool,
    ) -> Result<RecordInfo> {
        if self.recorder.as_ref().map(|r| r.is_running()).unwrap_or(false) {
            bail!("already recording {}", self.recorder.as_ref().unwrap().path().display());
        }
        let desktop = self.capture.desktop_rect()?;
        let (factor, w, h) = recorder::plan_size(desktop.width, desktop.height, max_side);
        let base = match name {
            Some(n) if !n.is_empty() => sanitize(&n),
            _ => format!("rec-{}", unix_time()),
        };
        let path = self.rec.dir.join(format!("{base}.mp4"));
        let opts = RecordOpts {
            path,
            fps: fps.clamp(1, 60),
            factor,
            width: w,
            height: h,
            bitrate_kbps: bitrate_kbps.clamp(200, 80_000),
            max_seconds: max_seconds.clamp(1, 6 * 3600),
            cursor,
        };
        let rec = match self.rec.zero_copy {
            true => match Recorder::start_zero_copy(opts.clone()) {
                Ok(r) => r,
                Err(e) => {
                    log::warn!("recorder: zero-copy start failed ({e:#}), trying GStreamer");
                    Recorder::start(self.gst_codec()?, opts)?
                }
            },
            false => Recorder::start(self.gst_codec()?, opts)?,
        };
        let info = rec.info();
        self.recorder = Some(rec);
        self.last_info = None;
        // First frame right away.
        self.tick();
        Ok(info)
    }
}

impl CaptureWorker {
    fn codec(&self) -> Option<Codec> {
        self.rec.codec.lock().unwrap().flatten()
    }

    fn gst_codec(&self) -> Result<Codec> {
        match *self.rec.codec.lock().unwrap() {
            None => bail!("encoder detection still running, retry in a few seconds"),
            Some(None) => bail!("recording disabled: no usable GStreamer H.264 encoder"),
            Some(Some(c)) => Ok(c),
        }
    }
}

fn rec_dim(r: &Recorder) -> (u32, u32) {
    let i = r.info();
    (i.width, i.height)
}

fn sanitize(name: &str) -> String {
    name.chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.' { c } else { '_' })
        .take(80)
        .collect()
}

fn unix_time() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn chown_path(path: &Path, uid: u32) {
    if let Ok(c) = std::ffi::CString::new(path.as_os_str().as_encoded_bytes()) {
        unsafe {
            libc::chown(c.as_ptr(), uid, u32::MAX);
        }
    }
}

pub struct Daemon {
    capture_tx: Sender<CaptureCmd>,
    pub input: Input,
    record_dir: String,
    started: Instant,
    served: u64,
}

impl Daemon {
    pub fn new(capture: Capturer, input: Input, rec: RecordConfig) -> Self {
        let record_dir = rec.dir.display().to_string();
        if let Err(e) = fs::create_dir_all(&rec.dir) {
            log::warn!("create {}: {e}", rec.dir.display());
        } else if let Some(uid) = rec.owner_uid {
            let _ = fs::set_permissions(&rec.dir, fs::Permissions::from_mode(0o755));
            chown_path(&rec.dir, uid);
        }
        let (tx, rx) = mpsc::channel();
        let worker = CaptureWorker {
            capture,
            rec,
            recorder: None,
            last_info: None,
        };
        std::thread::Builder::new()
            .name("capture".into())
            .spawn(move || worker.run(rx))
            .expect("spawn capture thread");
        Self {
            capture_tx: tx,
            input,
            record_dir,
            started: Instant::now(),
            served: 0,
        }
    }

    fn ask<T>(&self, make: impl FnOnce(Sender<T>) -> CaptureCmd) -> Result<T> {
        let (tx, rx) = mpsc::channel();
        self.capture_tx
            .send(make(tx))
            .map_err(|_| anyhow!("capture thread gone"))?;
        rx.recv_timeout(Duration::from_secs(30))
            .map_err(|_| anyhow!("capture thread did not answer in 30 s"))
    }

    /// One screenshot, used by `--check`.
    pub fn screenshot(&self, max_side: Option<u32>) -> Result<(serde_json::Value, Vec<u8>)> {
        self.ask(|reply| CaptureCmd::Screenshot {
            cursor: true,
            region: None,
            max_side,
            format: ImageFormat::Png,
            quality: None,
            reply,
        })?
    }

    pub fn serve(&mut self, socket: &Path, group: Option<&str>, owner: Option<&str>) -> Result<()> {
        if let Some(dir) = socket.parent() {
            fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;
        }
        let _ = fs::remove_file(socket);
        let listener = UnixListener::bind(socket)
            .with_context(|| format!("bind {}", socket.display()))?;
        fs::set_permissions(socket, fs::Permissions::from_mode(0o660))?;
        chown_socket(socket, owner, group)?;
        log::info!("listening on {}", socket.display());
        for conn in listener.incoming() {
            match conn {
                Ok(stream) => {
                    if let Err(e) = self.handle(stream) {
                        log::warn!("connection error: {e:#}");
                    }
                }
                Err(e) => log::warn!("accept: {e}"),
            }
        }
        Ok(())
    }

    fn handle(&mut self, mut stream: UnixStream) -> Result<()> {
        stream.set_read_timeout(Some(Duration::from_secs(10)))?;
        stream.set_write_timeout(Some(Duration::from_secs(10)))?;
        let raw = read_json_frame(&mut stream)?;
        let req: Request = match serde_json::from_slice(&raw) {
            Ok(r) => r,
            Err(e) => {
                return reply_err(&mut stream, format!("bad request: {e}"));
            }
        };
        self.served += 1;
        let t0 = Instant::now();
        let name = op_name(&req);
        let result = self.dispatch(req);
        let ms = t0.elapsed().as_secs_f32() * 1000.0;
        match result {
            Ok((data, payload)) => {
                log::debug!("{name}: ok in {ms:.1} ms, {} payload bytes", payload.len());
                let header = Header {
                    ok: true,
                    error: None,
                    data,
                    bytes: payload.len() as u64,
                };
                write_frame(&mut stream, &serde_json::to_vec(&header)?, &payload)
            }
            Err(e) => {
                log::warn!("{name}: {e:#} ({ms:.1} ms)");
                reply_err(&mut stream, format!("{e:#}"))
            }
        }
    }

    fn dispatch(&mut self, req: Request) -> Result<(serde_json::Value, Vec<u8>)> {
        use Request::*;
        let empty = Vec::new();
        Ok(match req {
            Ping => (json!({"pong": true}), empty),
            Status => (serde_json::to_value(self.status()?)?, empty),
            Displays => (self.ask(CaptureCmd::Displays)??, empty),
            Screenshot {
                max_side,
                format,
                quality,
                cursor,
                region,
            } => self.ask(|reply| CaptureCmd::Screenshot {
                cursor: cursor.unwrap_or(true),
                region,
                max_side,
                format,
                quality,
                reply,
            })??,
            Cursor => (self.ask(CaptureCmd::Cursor)??, empty),
            PointerMove { x, y } => {
                self.input.move_to(x, y)?;
                (json!({}), empty)
            }
            PointerButton { button, down } => {
                self.input.button(button, down)?;
                (json!({}), empty)
            }
            Click {
                x,
                y,
                button,
                count,
                modifiers,
            } => {
                self.input
                    .with_modifiers(&modifiers, |i| i.click(x, y, button, count))?;
                (json!({}), empty)
            }
            Drag {
                from_x,
                from_y,
                to_x,
                to_y,
                button,
                steps,
                modifiers,
            } => {
                self.input.with_modifiers(&modifiers, |i| {
                    i.drag((from_x, from_y), (to_x, to_y), button, steps)
                })?;
                (json!({}), empty)
            }
            Scroll {
                x,
                y,
                dx,
                dy,
                modifiers,
            } => {
                self.input
                    .with_modifiers(&modifiers, |i| i.scroll(x, y, dx, dy))?;
                (json!({}), empty)
            }
            KeyHold { keys, seconds } => {
                self.input.hold(&keys, seconds)?;
                (json!({}), empty)
            }
            Key { code, down } => {
                self.input.key(code, down)?;
                (json!({}), empty)
            }
            KeyCombo { keys } => {
                self.input.combo(&keys)?;
                (json!({}), empty)
            }
            Type { text, delay_ms } => {
                self.input.type_text(&text, delay_ms.unwrap_or(12))?;
                (json!({"typed": text.chars().count()}), empty)
            }
            Wake => {
                self.input.wake()?;
                (json!({}), empty)
            }
            RecordStart {
                name,
                fps,
                max_side,
                bitrate_kbps,
                max_seconds,
                cursor,
            } => {
                let info = self.ask(|reply| CaptureCmd::RecordStart {
                    name,
                    fps: fps.unwrap_or(15),
                    max_side: max_side.unwrap_or(1920),
                    bitrate_kbps: bitrate_kbps.unwrap_or(6000),
                    max_seconds: max_seconds.unwrap_or(600),
                    cursor: cursor.unwrap_or(true),
                    reply,
                })??;
                (serde_json::to_value(info)?, empty)
            }
            RecordStop => {
                let info = self.ask(CaptureCmd::RecordStop)??;
                (serde_json::to_value(info)?, empty)
            }
            RecordStatus => {
                let info = self.ask(CaptureCmd::RecordStatus)?;
                (serde_json::to_value(info)?, empty)
            }
        })
    }

    fn status(&mut self) -> Result<Status> {
        let displays = self.ask(CaptureCmd::Displays)??;
        let (gpu_driver, cursor_supported, record_codec) = self.ask(CaptureCmd::Info)?;
        Ok(Status {
            version: env!("CARGO_PKG_VERSION").to_string(),
            uptime_s: self.started.elapsed().as_secs(),
            gpu_driver,
            displays: serde_json::from_value(displays)?,
            desktop: self.input.desktop(),
            pointer_device: self.input.names().0,
            keyboard_device: self.input.names().1,
            cursor_supported,
            requests_served: self.served,
            record_dir: record_codec.as_ref().map(|_| self.record_dir.clone()),
            record_codec,
        })
    }
}

fn reply_err(stream: &mut UnixStream, message: String) -> Result<()> {
    let header = Header {
        ok: false,
        error: Some(message),
        data: serde_json::Value::Null,
        bytes: 0,
    };
    write_frame(stream, &serde_json::to_vec(&header)?, &[])?;
    stream.flush()?;
    Ok(())
}

fn op_name(req: &Request) -> &'static str {
    use Request::*;
    match req {
        Ping => "ping",
        Status => "status",
        Displays => "displays",
        Screenshot { .. } => "screenshot",
        Cursor => "cursor",
        PointerMove { .. } => "pointer_move",
        PointerButton { .. } => "pointer_button",
        Click { .. } => "click",
        Drag { .. } => "drag",
        Scroll { .. } => "scroll",
        Key { .. } => "key",
        KeyCombo { .. } => "key_combo",
        KeyHold { .. } => "key_hold",
        Type { .. } => "type",
        Wake => "wake",
        RecordStart { .. } => "record_start",
        RecordStop => "record_stop",
        RecordStatus => "record_status",
    }
}

pub fn lookup_uid(user: &str) -> Result<u32> {
    let cname = std::ffi::CString::new(user)?;
    let pw = unsafe { libc::getpwnam(cname.as_ptr()) };
    if pw.is_null() {
        bail!("user {user} does not exist");
    }
    Ok(unsafe { (*pw).pw_uid })
}

fn chown_socket(path: &Path, owner: Option<&str>, group: Option<&str>) -> Result<()> {
    let mut uid = u32::MAX;
    let mut gid = u32::MAX;
    if let Some(u) = owner {
        uid = lookup_uid(u)?;
    }
    if let Some(g) = group {
        let cname = std::ffi::CString::new(g)?;
        let gr = unsafe { libc::getgrnam(cname.as_ptr()) };
        if gr.is_null() {
            bail!("group {g} does not exist (groupadd {g})");
        }
        gid = unsafe { (*gr).gr_gid };
    }
    if uid == u32::MAX && gid == u32::MAX {
        return Ok(());
    }
    let cpath = std::ffi::CString::new(path.as_os_str().as_encoded_bytes())?;
    let rc = unsafe { libc::chown(cpath.as_ptr(), uid, gid) };
    if rc != 0 {
        return Err(std::io::Error::last_os_error()).context("chown socket");
    }
    Ok(())
}
