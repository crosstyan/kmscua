//! Screen recording: a fixed-rate frame loop feeding raw RGBA into a
//! GStreamer child process. Hardware H.264 where the platform has it
//! (Jetson `nvv4l2h264enc`, desktop NVENC `nvh264enc`), x264 otherwise.
//!
//! Frames are box-downscaled by an integer factor in-process (3840 -> 1920
//! is a 2x2 average, a few ms), then written to the child's stdin. The loop
//! writes exactly one frame per tick; if a grab fails the previous frame is
//! repeated, so wall-clock time and video time stay equal.
//!
//! With the `jetson` feature, a zero-copy sink replaces the child process
//! when the scanout can be imported by the VIC (see `crate::jetson`): the
//! loop then hands over scanout dma-bufs instead of RGBA.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Context, Result};
use image::RgbaImage;
use kmscua_proto::RecordInfo;

use crate::capture::{Capturer, Scanout};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Codec {
    /// Jetson / L4T V4L2 NVENC.
    NvV4l2H264,
    /// Desktop NVIDIA NVENC.
    NvH264,
    X264,
    OpenH264,
}

impl Codec {
    pub fn name(self) -> &'static str {
        match self {
            Codec::NvV4l2H264 => "nvv4l2h264enc",
            Codec::NvH264 => "nvh264enc",
            Codec::X264 => "x264enc",
            Codec::OpenH264 => "openh264enc",
        }
    }

    pub fn from_name(s: &str) -> Option<Self> {
        match s {
            "nvv4l2h264enc" | "nvv4l2" | "jetson" => Some(Codec::NvV4l2H264),
            "nvh264enc" | "nvenc" => Some(Codec::NvH264),
            "x264enc" | "x264" | "software" => Some(Codec::X264),
            "openh264enc" | "openh264" => Some(Codec::OpenH264),
            _ => None,
        }
    }

    pub fn hardware(self) -> bool {
        matches!(self, Codec::NvV4l2H264 | Codec::NvH264)
    }
}

fn gst_has(element: &str) -> bool {
    Command::new("gst-inspect-1.0")
        .arg(element)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// Pick the best available encoder, or honour an explicit request.
pub fn detect(preferred: Option<&str>) -> Option<Codec> {
    if !gst_has("fdsrc") || !gst_has("rawvideoparse") || !gst_has("mp4mux") || !gst_has("h264parse") {
        log::warn!("gstreamer base/good/bad plugins missing; recording disabled");
        return None;
    }
    if let Some(p) = preferred {
        let c = Codec::from_name(p)?;
        return if gst_has(c.name()) { Some(c) } else { None };
    }
    let order = [
        (Codec::NvV4l2H264, "nvvidconv"),
        (Codec::NvH264, "videoconvert"),
        (Codec::X264, "videoconvert"),
        (Codec::OpenH264, "videoconvert"),
    ];
    order
        .iter()
        .find(|(c, conv)| gst_has(c.name()) && gst_has(conv))
        .map(|(c, _)| *c)
}

/// Codec name the zero-copy backend is selected and reported by.
#[cfg(feature = "jetson")]
pub const ZERO_COPY: &str = crate::jetson::NAME;
#[cfg(not(feature = "jetson"))]
pub const ZERO_COPY: &str = "jetson-zc";

/// Whether zero-copy recording can serve this capture: built with the
/// `jetson` feature, not overridden by `preferred`, and the scanout imports.
pub fn zero_copy_probe(capture: &mut Capturer, preferred: Option<&str>) -> bool {
    #[cfg(feature = "jetson")]
    if crate::jetson::wanted(preferred) {
        match crate::jetson::probe(capture) {
            Ok(()) => return true,
            Err(e) => log::info!("zero-copy recording unavailable: {e:#}"),
        }
    }
    #[cfg(not(feature = "jetson"))]
    if preferred == Some(ZERO_COPY) {
        log::warn!("{ZERO_COPY} requested but cuad was built without the jetson feature");
    }
    let _ = (capture, preferred);
    false
}

#[derive(Clone)]
pub struct RecordOpts {
    pub path: PathBuf,
    pub fps: u32,
    pub factor: u32,
    pub width: u32,
    pub height: u32,
    pub bitrate_kbps: u32,
    pub max_seconds: u32,
    pub cursor: bool,
}

enum Sink {
    /// gst-launch child reading RGBA on stdin; `stdin` is None once closed.
    Gst {
        child: Child,
        stdin: Option<ChildStdin>,
        last: Vec<u8>,
    },
    /// Some while recording.
    #[cfg(feature = "jetson")]
    ZeroCopy(Option<crate::jetson::ZeroCopyEncoder>),
}

pub struct Recorder {
    sink: Sink,
    opts: RecordOpts,
    codec: &'static str,
    started: Instant,
    frames: u64,
    dropped: u64,
    next_tick: Instant,
    finalized: Option<bool>,
    error: Option<String>,
}

impl Recorder {
    pub fn start(codec: Codec, opts: RecordOpts) -> Result<Self> {
        if let Some(dir) = opts.path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let (w, h, fps) = (opts.width, opts.height, opts.fps);
        let mut args: Vec<String> = vec![
            "-q".into(),
            "fdsrc".into(),
            "fd=0".into(),
            format!("blocksize={}", w * h * 4),
            "!".into(),
            "rawvideoparse".into(),
            "use-sink-caps=false".into(),
            "format=rgba".into(),
            format!("width={w}"),
            format!("height={h}"),
            format!("framerate={fps}/1"),
            "!".into(),
        ];
        let gop = (fps * 2).max(1);
        let tail: Vec<String> = match codec {
            Codec::NvV4l2H264 => vec![
                "nvvidconv".into(),
                "!".into(),
                "video/x-raw(memory:NVMM),format=NV12".into(),
                "!".into(),
                "nvv4l2h264enc".into(),
                format!("bitrate={}", opts.bitrate_kbps * 1000),
                "insert-sps-pps=true".into(),
                format!("iframeinterval={gop}"),
                format!("idrinterval={gop}"),
                "!".into(),
                "h264parse".into(),
            ],
            Codec::NvH264 => vec![
                "videoconvert".into(),
                "!".into(),
                "nvh264enc".into(),
                format!("bitrate={}", opts.bitrate_kbps),
                format!("gop-size={gop}"),
                "!".into(),
                "h264parse".into(),
            ],
            Codec::X264 => vec![
                "videoconvert".into(),
                "!".into(),
                "x264enc".into(),
                "speed-preset=ultrafast".into(),
                "tune=zerolatency".into(),
                format!("bitrate={}", opts.bitrate_kbps),
                format!("key-int-max={gop}"),
                "!".into(),
                "h264parse".into(),
            ],
            Codec::OpenH264 => vec![
                "videoconvert".into(),
                "!".into(),
                "openh264enc".into(),
                format!("bitrate={}", opts.bitrate_kbps * 1000),
                format!("gop-size={gop}"),
                "!".into(),
                "h264parse".into(),
            ],
        };
        args.extend(tail);
        args.extend(
            [
                "!",
                "mp4mux",
                "!",
                "filesink",
                &format!("location={}", opts.path.display()),
            ]
            .iter()
            .map(|s| s.to_string()),
        );
        log::info!("recorder: gst-launch-1.0 {}", args.join(" "));
        let mut child = Command::new("gst-launch-1.0")
            .args(&args)
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .context("spawn gst-launch-1.0")?;
        let stdin = child.stdin.take();
        Ok(Self::with_sink(
            Sink::Gst {
                child,
                stdin,
                last: Vec::new(),
            },
            codec.name(),
            opts,
        ))
    }

    /// Record through the Jetson VIC + NVENC, straight from scanout dma-bufs.
    /// Feed it with `push_scanout`.
    pub fn start_zero_copy(opts: RecordOpts) -> Result<Self> {
        #[cfg(feature = "jetson")]
        {
            if let Some(dir) = opts.path.parent() {
                std::fs::create_dir_all(dir)?;
            }
            let enc = crate::jetson::ZeroCopyEncoder::create(
                &opts.path,
                opts.width,
                opts.height,
                opts.factor,
                opts.fps,
                opts.bitrate_kbps,
            )?;
            log::info!("recorder: zero-copy VIC + NVENC H.264 {}x{} -> {}", opts.width, opts.height, opts.path.display());
            Ok(Self::with_sink(Sink::ZeroCopy(Some(enc)), ZERO_COPY, opts))
        }
        #[cfg(not(feature = "jetson"))]
        {
            let _ = opts;
            bail!("built without the jetson feature")
        }
    }

    fn with_sink(sink: Sink, codec: &'static str, opts: RecordOpts) -> Self {
        let now = Instant::now();
        Self {
            sink,
            opts,
            codec,
            started: now,
            frames: 0,
            dropped: 0,
            next_tick: now,
            finalized: None,
            error: None,
        }
    }

    /// True when frames go in as scanout dma-bufs (`push_scanout`), not RGBA.
    pub fn zero_copy(&self) -> bool {
        !matches!(self.sink, Sink::Gst { .. })
    }

    pub fn wants_cursor(&self) -> bool {
        self.opts.cursor
    }

    pub fn factor(&self) -> u32 {
        self.opts.factor
    }

    /// When the next frame is due.
    pub fn next_tick(&self) -> Instant {
        self.next_tick
    }

    pub fn is_running(&self) -> bool {
        match &self.sink {
            Sink::Gst { stdin, .. } => stdin.is_some(),
            #[cfg(feature = "jetson")]
            Sink::ZeroCopy(enc) => enc.is_some(),
        }
    }

    pub fn over_time(&self) -> bool {
        self.started.elapsed().as_secs() >= self.opts.max_seconds as u64
    }

    /// Feed one frame (already downscaled to opts.width x opts.height RGBA).
    /// Pass None to repeat the previous frame.
    pub fn push(&mut self, frame: Option<&RgbaImage>) -> Result<()> {
        self.advance_tick();
        let (w, h) = (self.opts.width, self.opts.height);
        let (pipe, last) = match &mut self.sink {
            Sink::Gst { stdin, last, .. } => (stdin, last),
            #[cfg(feature = "jetson")]
            Sink::ZeroCopy(_) => bail!("zero-copy recorder takes scanouts"),
        };
        let Some(stdin) = pipe.as_mut() else {
            bail!("recorder stopped");
        };
        let bytes: &[u8] = match frame {
            Some(img) => {
                let expect = (w * h * 4) as usize;
                if img.as_raw().len() != expect {
                    bail!("frame size mismatch: {} vs {}", img.as_raw().len(), expect);
                }
                last.clear();
                last.extend_from_slice(img.as_raw());
                last
            }
            None => {
                self.dropped += 1;
                if last.is_empty() {
                    *last = vec![0u8; (w * h * 4) as usize];
                }
                last
            }
        };
        if let Err(e) = stdin.write_all(bytes) {
            self.error = Some(format!("encoder pipe: {e}"));
            *pipe = None;
            return Err(anyhow!("encoder pipe closed: {e}"));
        }
        self.frames += 1;
        Ok(())
    }

    /// Feed one scanout to the zero-copy sink. None repeats the previous one.
    pub fn push_scanout(&mut self, frame: Option<&Scanout>) -> Result<()> {
        self.advance_tick();
        #[cfg(feature = "jetson")]
        if let Sink::ZeroCopy(slot) = &mut self.sink {
            let Some(enc) = slot.as_mut() else {
                bail!("recorder stopped");
            };
            match enc.push(frame) {
                Ok(true) => {}
                Ok(false) => return Ok(()), // nothing grabbed yet to repeat
                Err(e) => {
                    self.error = Some(format!("{e:#}"));
                    *slot = None;
                    return Err(e);
                }
            }
            self.frames += 1;
            if frame.is_none() {
                self.dropped += 1;
            }
            return Ok(());
        }
        let _ = frame;
        bail!("recorder takes RGBA frames")
    }

    /// Schedule the next tick from the previous deadline, not from now, so
    /// the average rate stays exact even when a grab runs long.
    fn advance_tick(&mut self) {
        let period = Duration::from_secs_f64(1.0 / self.opts.fps as f64);
        self.next_tick += period;
        if self.next_tick < Instant::now() - period {
            // We fell far behind (encoder stall); resync instead of bursting.
            self.next_tick = Instant::now() + period;
        }
    }

    /// Close the pipe (EOS), wait for the muxer to write the MP4 trailer.
    pub fn stop(&mut self) -> RecordInfo {
        let (child, stdin) = match &mut self.sink {
            Sink::Gst { child, stdin, .. } => (child, stdin),
            #[cfg(feature = "jetson")]
            Sink::ZeroCopy(slot) => {
                let result = match slot.take() {
                    Some(enc) => enc.finish().map(|s| {
                        log::info!(
                            "recorder: zero-copy {} frames, capture->bitstream {:.1} ms mean {:.1} ms max, VIC {:.2} ms",
                            s.frames,
                            s.latency_ms_mean,
                            s.latency_ms_max,
                            s.convert_ms_mean
                        );
                    }),
                    None => Err(anyhow!("encoder already closed")),
                };
                if let Err(e) = &result {
                    self.error.get_or_insert(format!("{e:#}"));
                }
                self.finalized = Some(result.is_ok() && self.opts.path.is_file());
                return self.info();
            }
        };
        drop(stdin.take());
        let deadline = Instant::now() + Duration::from_secs(15);
        let mut status = None;
        while Instant::now() < deadline {
            match child.try_wait() {
                Ok(Some(s)) => {
                    status = Some(s);
                    break;
                }
                Ok(None) => std::thread::sleep(Duration::from_millis(50)),
                Err(e) => {
                    self.error = Some(format!("wait: {e}"));
                    break;
                }
            }
        }
        if status.is_none() {
            let _ = child.kill();
            let _ = child.wait();
            self.error.get_or_insert("encoder did not finish in 15 s; file may be truncated".into());
        }
        let ok = status.map(|s| s.success()).unwrap_or(false);
        if !ok {
            if let Some(mut err) = child.stderr.take() {
                let mut s = String::new();
                let _ = std::io::Read::read_to_string(&mut err, &mut s);
                let tail: String = s.lines().rev().take(3).collect::<Vec<_>>().join(" | ");
                if !tail.is_empty() {
                    self.error.get_or_insert(tail);
                }
            }
        }
        self.finalized = Some(ok && self.opts.path.is_file());
        self.info()
    }

    pub fn info(&self) -> RecordInfo {
        RecordInfo {
            recording: self.is_running(),
            path: Some(self.opts.path.display().to_string()),
            codec: Some(self.codec.to_string()),
            width: self.opts.width,
            height: self.opts.height,
            fps: self.opts.fps,
            frames: self.frames,
            dropped: self.dropped,
            seconds: self.frames as f32 / self.opts.fps as f32,
            finalized: self.finalized,
            error: self.error.clone(),
        }
    }

    pub fn path(&self) -> &Path {
        &self.opts.path
    }
}

/// Integer box downscale, shared with screenshots.
pub fn downscale(src: &RgbaImage, factor: u32) -> RgbaImage {
    crate::capture::downscale_box(src, factor)
}

/// Choose the integer factor so the longest side fits `max_side`, and the
/// resulting size is even (encoders want even dimensions).
pub fn plan_size(sw: u32, sh: u32, max_side: u32) -> (u32, u32, u32) {
    let longest = sw.max(sh);
    let mut factor = 1;
    while longest / factor > max_side.max(16) {
        factor += 1;
    }
    let w = (sw / factor) & !1;
    let h = (sh / factor) & !1;
    (factor, w.max(2), h.max(2))
}

#[cfg(all(test, feature = "jetson"))]
mod tests {
    use super::*;

    fn cpu_seconds() -> f64 {
        let mut ru: libc::rusage = unsafe { std::mem::zeroed() };
        unsafe { libc::getrusage(libc::RUSAGE_SELF, &mut ru) };
        let t = |tv: libc::timeval| tv.tv_sec as f64 + tv.tv_usec as f64 / 1e6;
        t(ru.ru_utime) + t(ru.ru_stime)
    }

    /// Records the live scanout through the zero-copy path with the daemon's
    /// tick loop and prints CPU use. Run as root on a Jetson:
    /// KMSCUA_ZC_DEVICE=/dev/dri/by-path/platform-13800000.display-card \
    ///   KMSCUA_ZC_OUT=/path/out.mp4 <test binary> --ignored record_scanout --nocapture
    #[test]
    #[ignore = "needs root, a Jetson and an importable scanout"]
    fn record_scanout() {
        let _ = env_logger::builder().is_test(true).filter_level(log::LevelFilter::Info).try_init();
        let env = |k: &str| std::env::var(k).ok();
        let seconds: u32 = env("KMSCUA_ZC_SECONDS").and_then(|s| s.parse().ok()).unwrap_or(10);
        let max_side: u32 = env("KMSCUA_ZC_MAX_SIDE").and_then(|s| s.parse().ok()).unwrap_or(1920);
        let fps: u32 = env("KMSCUA_ZC_FPS").and_then(|s| s.parse().ok()).unwrap_or(30);
        let path = PathBuf::from(env("KMSCUA_ZC_OUT").unwrap_or_else(|| "/tmp/kmscua-zc.mp4".into()));
        let mut cap = Capturer::open(env("KMSCUA_ZC_DEVICE"), 0, None).expect("open capture");
        assert!(zero_copy_probe(&mut cap, None), "scanout not importable");
        let desktop = cap.desktop_rect().unwrap();
        let (factor, width, height) = plan_size(desktop.width, desktop.height, max_side);
        let opts = RecordOpts {
            path: path.clone(),
            fps,
            factor,
            width,
            height,
            bitrate_kbps: 8000,
            max_seconds: seconds,
            cursor: env("KMSCUA_ZC_CURSOR").as_deref() != Some("0"),
        };
        let mut rec = Recorder::start_zero_copy(opts).expect("start");
        let (cpu0, t0) = (cpu_seconds(), Instant::now());
        let mut work = Duration::ZERO;
        while !rec.over_time() {
            std::thread::sleep(rec.next_tick().saturating_duration_since(Instant::now()));
            let w0 = Instant::now();
            let shot = cap.grab_scanout(rec.wants_cursor()).ok();
            rec.push_scanout(shot.as_ref()).expect("push");
            work += w0.elapsed();
        }
        let (cpu, wall) = (cpu_seconds() - cpu0, t0.elapsed().as_secs_f64());
        let info = rec.stop();
        println!(
            "{}x{} -> {}x{} @ {fps} fps: {} frames ({} repeated) in {wall:.1} s, CPU {:.1}% of one core, \
             {:.2} ms per tick on the worker, finalized={:?} error={:?} -> {}",
            desktop.width,
            desktop.height,
            width,
            height,
            info.frames,
            info.dropped,
            100.0 * cpu / wall,
            work.as_secs_f64() * 1000.0 / info.frames.max(1) as f64,
            info.finalized,
            info.error,
            path.display()
        );
        assert_eq!(info.finalized, Some(true));
    }
}
