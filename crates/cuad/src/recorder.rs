//! Screen recording: a fixed-rate frame loop feeding raw RGBA into a
//! GStreamer child process. Hardware H.264 where the platform has it
//! (Jetson `nvv4l2h264enc`, desktop NVENC `nvh264enc`), x264 otherwise.
//!
//! Frames are box-downscaled by an integer factor in-process (3840 -> 1920
//! is a 2x2 average, a few ms), then written to the child's stdin. The loop
//! writes exactly one frame per tick; if a grab fails the previous frame is
//! repeated, so wall-clock time and video time stay equal.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Context, Result};
use image::RgbaImage;
use kmscua_proto::RecordInfo;

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

pub struct Recorder {
    child: Child,
    stdin: Option<ChildStdin>,
    opts: RecordOpts,
    codec: Codec,
    started: Instant,
    frames: u64,
    dropped: u64,
    last: Vec<u8>,
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
        let now = Instant::now();
        Ok(Self {
            child,
            stdin,
            opts,
            codec,
            started: now,
            frames: 0,
            dropped: 0,
            last: Vec::new(),
            next_tick: now,
            finalized: None,
            error: None,
        })
    }

    pub fn codec(&self) -> Codec {
        self.codec
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
        self.stdin.is_some()
    }

    pub fn over_time(&self) -> bool {
        self.started.elapsed().as_secs() >= self.opts.max_seconds as u64
    }

    /// Feed one frame (already downscaled to opts.width x opts.height RGBA).
    /// Pass None to repeat the previous frame.
    pub fn push(&mut self, frame: Option<&RgbaImage>) -> Result<()> {
        let period = Duration::from_secs_f64(1.0 / self.opts.fps as f64);
        // Schedule the next tick from the previous deadline, not from now, so
        // the average rate stays exact even when a grab runs long.
        self.next_tick += period;
        if self.next_tick < Instant::now() - period {
            // We fell far behind (encoder stall); resync instead of bursting.
            self.next_tick = Instant::now() + period;
        }
        let Some(stdin) = self.stdin.as_mut() else {
            bail!("recorder stopped");
        };
        let bytes: &[u8] = match frame {
            Some(img) => {
                let expect = (self.opts.width * self.opts.height * 4) as usize;
                if img.as_raw().len() != expect {
                    bail!("frame size mismatch: {} vs {}", img.as_raw().len(), expect);
                }
                self.last.clear();
                self.last.extend_from_slice(img.as_raw());
                &self.last
            }
            None => {
                self.dropped += 1;
                if self.last.is_empty() {
                    self.last = vec![0u8; (self.opts.width * self.opts.height * 4) as usize];
                }
                &self.last
            }
        };
        if let Err(e) = stdin.write_all(bytes) {
            self.error = Some(format!("encoder pipe: {e}"));
            self.stdin = None;
            return Err(anyhow!("encoder pipe closed: {e}"));
        }
        self.frames += 1;
        Ok(())
    }

    /// Close the pipe (EOS), wait for the muxer to write the MP4 trailer.
    pub fn stop(&mut self) -> RecordInfo {
        drop(self.stdin.take());
        let deadline = Instant::now() + Duration::from_secs(15);
        let mut status = None;
        while Instant::now() < deadline {
            match self.child.try_wait() {
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
            let _ = self.child.kill();
            let _ = self.child.wait();
            self.error.get_or_insert("encoder did not finish in 15 s; file may be truncated".into());
        }
        let ok = status.map(|s| s.success()).unwrap_or(false);
        if !ok {
            if let Some(mut err) = self.child.stderr.take() {
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
            codec: Some(self.codec.name().to_string()),
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

/// Integer box downscale by `factor` (average of factor x factor blocks).
pub fn downscale(src: &RgbaImage, factor: u32) -> RgbaImage {
    if factor <= 1 {
        return src.clone();
    }
    let (sw, sh) = src.dimensions();
    let (dw, dh) = (sw / factor, sh / factor);
    let mut dst = RgbaImage::new(dw, dh);
    let s = src.as_raw();
    let d = dst.as_mut();
    let n = (factor * factor) as u32;
    let half = n / 2;
    for y in 0..dh {
        for x in 0..dw {
            let mut acc = [0u32; 3];
            for dy in 0..factor {
                let row = ((y * factor + dy) * sw) as usize * 4;
                for dx in 0..factor {
                    let i = row + ((x * factor + dx) as usize) * 4;
                    acc[0] += s[i] as u32;
                    acc[1] += s[i + 1] as u32;
                    acc[2] += s[i + 2] as u32;
                }
            }
            let o = ((y * dw + x) * 4) as usize;
            d[o] = ((acc[0] + half) / n) as u8;
            d[o + 1] = ((acc[1] + half) / n) as u8;
            d[o + 2] = ((acc[2] + half) / n) as u8;
            d[o + 3] = 255;
        }
    }
    dst
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
