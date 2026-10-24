//! Recording as a directory of JPEG stills, in the layout
//! `kmscua_mcp::stills` reads (see its module doc for the contract).
//!
//! A thread grabs the display at a fixed rate ([`DEFAULT_FPS`] unless the
//! request says otherwise), box-downscales by an integer factor so the
//! longest side fits `max_side`, and writes a still only when the picture
//! moved since the last still written: a still holds until the next one, so
//! an idle screen costs one file. Ticks are scheduled from the previous
//! deadline; a tick that passes while a grab or a write runs long is
//! counted in `dropped` and skipped, not burst.
//!
//! Disk: a 1920x1080 desktop at JPEG quality [`QUALITY`] is roughly
//! 100-300 KB a still, so a screen that changes on every tick at 5 fps
//! writes about 1 MB/s and the 600 s default cap bounds a recording near
//! 600 MB. An agent session, idle between actions, writes a small fraction
//! of that.

// main.rs wires this in once the X11 backend lands.

use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender, TryRecvError};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use image::RgbaImage;
use kmscua_proto::RecordInfo;

/// Grabs one full frame of the display, cursor composited when the flag is set.
/// Called from the recorder's own thread.
pub type Grab = Box<dyn FnMut(bool) -> Result<RgbaImage> + Send>;

/// The `Request::RecordStart` fields, unpacked.
#[derive(Debug, Clone, Default)]
pub struct RecordOpts {
    pub name: Option<String>,
    pub fps: Option<u32>,
    pub max_side: Option<u32>,
    pub max_seconds: Option<u32>,
    pub cursor: Option<bool>,
}

/// Capture rate when the request names none. Lower than the MP4 recorder's
/// 15: every changed tick is a JPEG encode and a file, and 5 fps still
/// catches a flicker of 200 ms.
pub const DEFAULT_FPS: u32 = 5;
/// Longest side of a still when the request names none, as for an MP4.
pub const DEFAULT_MAX_SIDE: u32 = 1920;
pub const DEFAULT_MAX_SECONDS: u32 = 600;
/// JPEG quality: text stays legible, a still stays a few hundred KB.
pub const QUALITY: u8 = 75;
/// A frame whose every channel is within this many levels of the last
/// still written is the same picture, and is not written.
const SAME: u8 = 4;
pub const CODEC: &str = "jpeg-stills";

/// What the thread updates and `info` reads.
#[derive(Default)]
struct Shared {
    recording: bool,
    frames: u64,
    dropped: u64,
    /// Final length, once the thread has ended.
    seconds: f32,
    finalized: Option<bool>,
    error: Option<String>,
}

pub struct Recorder {
    path: PathBuf,
    width: u32,
    height: u32,
    fps: u32,
    started: Instant,
    limit: Duration,
    shared: Arc<Mutex<Shared>>,
    stop_tx: Option<Sender<()>>,
    thread: Option<JoinHandle<()>>,
}

impl Recorder {
    /// Start a capture thread writing `<dir>/<name>.frames/<ms>.jpg`.
    pub fn start(dir: &Path, opts: RecordOpts, mut grab: Grab) -> Result<Self> {
        let base = match opts.name.as_deref() {
            Some(n) if !n.is_empty() => sanitize(n),
            _ => format!("rec-{}", unix_time()),
        };
        let path = dir.join(format!("{base}.frames"));
        std::fs::create_dir_all(&path).with_context(|| format!("create {}", path.display()))?;
        clear_stills(&path)?;
        let fps = opts.fps.unwrap_or(DEFAULT_FPS).clamp(1, 30);
        let max_side = opts.max_side.unwrap_or(DEFAULT_MAX_SIDE);
        let limit = Duration::from_secs(opts.max_seconds.unwrap_or(DEFAULT_MAX_SECONDS).clamp(1, 6 * 3600) as u64);
        let cursor = opts.cursor.unwrap_or(true);

        // The first still is the picture at start, written before we return
        // so a recording that answered OK always has `00000000.jpg`.
        let started = Instant::now();
        let first = grab(cursor).context("grab the first frame")?;
        let factor = plan_factor(first.width(), first.height(), max_side);
        let first = downscale_box(&first, factor);
        let (width, height) = first.dimensions();
        write_still(&path, 0, &first)?;

        let shared = Arc::new(Mutex::new(Shared { recording: true, frames: 1, ..Default::default() }));
        let (stop_tx, stop_rx) = mpsc::channel();
        let lp = Loop {
            path: path.clone(),
            grab,
            cursor,
            factor,
            size: (width, height),
            period: Duration::from_secs_f64(1.0 / fps as f64),
            limit,
            started,
            last: first,
            last_ms: 0,
            shared: shared.clone(),
        };
        let thread = std::thread::Builder::new()
            .name("xcua-record".into())
            .spawn(move || lp.run(stop_rx))
            .context("spawn the recorder thread")?;
        log::info!("recording {} at {width}x{height}, {fps} fps, factor {factor}", path.display());
        Ok(Self { path, width, height, fps, started, limit, shared, stop_tx: Some(stop_tx), thread: Some(thread) })
    }

    /// Current state; `recording: false` once stopped or past `max_seconds`.
    pub fn info(&self) -> RecordInfo {
        let s = self.shared.lock().unwrap();
        RecordInfo {
            recording: s.recording,
            path: Some(self.path.display().to_string()),
            codec: Some(CODEC.into()),
            width: self.width,
            height: self.height,
            fps: self.fps,
            frames: s.frames,
            dropped: s.dropped,
            seconds: if s.recording { self.started.elapsed().min(self.limit).as_secs_f32() } else { s.seconds },
            finalized: s.finalized,
            error: s.error.clone(),
        }
    }

    /// Stop, wait for the last still to be on disk, return the final info
    /// with `finalized: Some(true)`.
    pub fn stop(&mut self) -> RecordInfo {
        // Dropping the sender wakes the thread out of its sleep.
        drop(self.stop_tx.take());
        if let Some(t) = self.thread.take() {
            if t.join().is_err() {
                let mut s = self.shared.lock().unwrap();
                s.recording = false;
                s.seconds = self.started.elapsed().min(self.limit).as_secs_f32();
                s.error.get_or_insert("recorder thread panicked".into());
                // The stills already written are complete files.
                s.finalized = Some(true);
            }
        }
        self.info()
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for Recorder {
    fn drop(&mut self) {
        self.stop();
    }
}

/// The capture thread's state.
struct Loop {
    path: PathBuf,
    grab: Grab,
    cursor: bool,
    factor: u32,
    /// Every still is this size, even if the display is resized mid-recording.
    size: (u32, u32),
    period: Duration,
    limit: Duration,
    started: Instant,
    /// The last still written, as written (before JPEG).
    last: RgbaImage,
    last_ms: u64,
    shared: Arc<Mutex<Shared>>,
}

impl Loop {
    fn run(mut self, stop: Receiver<()>) {
        let mut next = self.started + self.period;
        loop {
            let now = Instant::now();
            let stopped = if next > now {
                !matches!(stop.recv_timeout(next - now), Err(RecvTimeoutError::Timeout))
            } else {
                !matches!(stop.try_recv(), Err(TryRecvError::Empty))
            };
            let at = self.started.elapsed();
            if stopped || at >= self.limit {
                break;
            }
            next += self.period;
            self.tick(at);
            // Ticks that went by while this one ran are dropped, not burst.
            let now = Instant::now();
            if now > next + self.period {
                let missed = ((now - next).as_secs_f64() / self.period.as_secs_f64()) as u32;
                next += self.period * missed;
                self.shared.lock().unwrap().dropped += missed as u64;
            }
        }
        self.finish();
    }

    fn tick(&mut self, at: Duration) {
        let img = match (self.grab)(self.cursor) {
            Ok(img) => fit(downscale_box(&img, self.factor), self.size),
            Err(e) => {
                let mut s = self.shared.lock().unwrap();
                s.dropped += 1;
                s.error = Some(format!("grab: {e:#}"));
                return;
            }
        };
        if same(&img, &self.last) {
            return;
        }
        let ms = at.as_millis() as u64;
        match write_still(&self.path, ms, &img) {
            Ok(()) => {
                self.last = img;
                self.last_ms = ms;
                self.shared.lock().unwrap().frames += 1;
            }
            Err(e) => self.shared.lock().unwrap().error = Some(format!("{e:#}")),
        }
    }

    /// A closing still at the stop time, a copy of the last, so the
    /// directory alone tells how long the recording ran.
    fn finish(self) {
        let end = self.started.elapsed().min(self.limit);
        let end_ms = end.as_millis() as u64;
        let mut wrote = false;
        let mut err = None;
        if end_ms > self.last_ms {
            match copy_still(&self.path, self.last_ms, end_ms) {
                Ok(()) => wrote = true,
                Err(e) => err = Some(format!("{e:#}")),
            }
        }
        let mut s = self.shared.lock().unwrap();
        s.frames += wrote as u64;
        if err.is_some() {
            s.error = err;
        }
        s.seconds = end.as_secs_f32();
        s.recording = false;
        s.finalized = Some(true);
        log::info!("recording {} ended: {:.1}s, {} stills, {} dropped", self.path.display(), s.seconds, s.frames, s.dropped);
    }
}

fn still_name(ms: u64) -> String {
    format!("{ms:08}.jpg")
}

/// Encode to a hidden temp file in the same directory, then rename, so a
/// reader never sees half a JPEG.
fn write_still(dir: &Path, ms: u64, img: &RgbaImage) -> Result<()> {
    let rgb = image::DynamicImage::ImageRgba8(img.clone()).into_rgb8();
    let mut buf = Vec::with_capacity(256 * 1024);
    image::codecs::jpeg::JpegEncoder::new_with_quality(&mut buf, QUALITY)
        .encode_image(&rgb)
        .context("encode JPEG")?;
    let name = still_name(ms);
    let tmp = dir.join(format!(".{name}.tmp"));
    std::fs::write(&tmp, &buf).with_context(|| format!("write {}", tmp.display()))?;
    std::fs::rename(&tmp, dir.join(&name)).with_context(|| format!("rename {}", tmp.display()))
}

fn copy_still(dir: &Path, from_ms: u64, to_ms: u64) -> Result<()> {
    let name = still_name(to_ms);
    let tmp = dir.join(format!(".{name}.tmp"));
    std::fs::copy(dir.join(still_name(from_ms)), &tmp).with_context(|| format!("write {}", tmp.display()))?;
    std::fs::rename(&tmp, dir.join(&name)).with_context(|| format!("rename {}", tmp.display()))
}

/// Remove the stills of an earlier recording under the same name, as an
/// MP4 of the same name is overwritten.
fn clear_stills(dir: &Path) -> Result<()> {
    for e in std::fs::read_dir(dir).with_context(|| format!("read {}", dir.display()))? {
        let p = e?.path();
        let ours = p.extension().and_then(|e| e.to_str()) == Some("jpg")
            && p.file_stem().and_then(|s| s.to_str()).is_some_and(|s| s.bytes().all(|b| b.is_ascii_digit()));
        if ours {
            std::fs::remove_file(&p).with_context(|| format!("remove {}", p.display()))?;
        }
    }
    Ok(())
}

fn same(a: &RgbaImage, b: &RgbaImage) -> bool {
    a.dimensions() == b.dimensions() && a.as_raw().iter().zip(b.as_raw()).all(|(x, y)| x.abs_diff(*y) <= SAME)
}

/// Bring a frame to the recording's size when the display changed size.
fn fit(img: RgbaImage, (w, h): (u32, u32)) -> RgbaImage {
    if img.dimensions() == (w, h) {
        img
    } else {
        image::imageops::resize(&img, w, h, image::imageops::FilterType::Triangle)
    }
}

/// Smallest integer factor that brings the longest side within `max_side`
/// (from cuad's `recorder::plan_size`; stills need no even dimensions).
fn plan_factor(sw: u32, sh: u32, max_side: u32) -> u32 {
    let longest = sw.max(sh);
    let mut factor = 1;
    while longest / factor > max_side.max(16) {
        factor += 1;
    }
    factor
}

/// Integer box downscale: each output pixel is the mean of a `factor` x
/// `factor` block. Copied from cuad's `capture::downscale_box` so xcua does
/// not depend on cuad.
fn downscale_box(src: &RgbaImage, factor: u32) -> RgbaImage {
    if factor <= 1 {
        return src.clone();
    }
    let (sw, sh) = src.dimensions();
    let (dw, dh) = ((sw / factor).max(1), (sh / factor).max(1));
    let s = src.as_raw();
    let mut out = vec![0u8; (dw * dh * 4) as usize];
    let n = factor * factor;
    let half = n / 2;
    let threads = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(1).min(dh as usize).max(1);
    let rows_per = (dh as usize).div_ceil(threads);
    std::thread::scope(|sc| {
        for (ti, chunk) in out.chunks_mut(rows_per * dw as usize * 4).enumerate() {
            let y0 = (ti * rows_per) as u32;
            sc.spawn(move || {
                let rows = chunk.len() / (dw as usize * 4);
                for ry in 0..rows as u32 {
                    let y = y0 + ry;
                    for x in 0..dw {
                        let mut acc = [0u32; 3];
                        for dy in 0..factor {
                            let row = ((y * factor + dy) * sw) as usize * 4;
                            let mut i = row + (x * factor) as usize * 4;
                            for _ in 0..factor {
                                acc[0] += s[i] as u32;
                                acc[1] += s[i + 1] as u32;
                                acc[2] += s[i + 2] as u32;
                                i += 4;
                            }
                        }
                        let o = ((ry * dw + x) * 4) as usize;
                        chunk[o] = ((acc[0] + half) / n) as u8;
                        chunk[o + 1] = ((acc[1] + half) / n) as u8;
                        chunk[o + 2] = ((acc[2] + half) / n) as u8;
                        chunk[o + 3] = 255;
                    }
                }
            });
        }
    });
    RgbaImage::from_raw(dw, dh, out).expect("sized buffer")
}

/// As cuad names recordings.
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};

    fn tmp(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("xcua-record-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    /// A display whose picture is `look(call number)`, a flat grey level.
    fn fake(w: u32, h: u32, calls: Arc<AtomicU32>, look: impl Fn(u32) -> u8 + Send + 'static) -> Grab {
        Box::new(move |_cursor| {
            let n = calls.fetch_add(1, Ordering::SeqCst);
            let v = look(n);
            Ok(RgbaImage::from_pixel(w, h, image::Rgba([v, v / 2, 255 - v, 255])))
        })
    }

    fn stills(dir: &Path) -> Vec<String> {
        let mut v: Vec<String> = std::fs::read_dir(dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        v.sort();
        v
    }

    #[test]
    fn static_then_changing() {
        let dir = tmp("dedupe");
        let calls = Arc::new(AtomicU32::new(0));
        // Calls 0..=8 show one picture (call 0 is the first still), then
        // every call a new one.
        let grab = fake(64, 48, calls.clone(), |n| if n <= 8 { 10 } else { (n * 20 % 250) as u8 });
        let opts = RecordOpts { name: Some("t 1".into()), fps: Some(20), ..Default::default() };
        let mut rec = Recorder::start(&dir, opts, grab).unwrap();
        let live = rec.info();
        assert!(live.recording);
        assert_eq!(live.finalized, None);
        std::thread::sleep(Duration::from_millis(900));
        let info = rec.stop();
        let path = dir.join("t_1.frames");
        assert_eq!(info.path.as_deref(), Some(path.to_str().unwrap()));
        assert!(!info.recording);
        assert_eq!(info.finalized, Some(true));
        assert_eq!(info.codec.as_deref(), Some("jpeg-stills"));
        assert_eq!((info.width, info.height, info.fps), (64, 48, 20));
        assert!(info.seconds >= 0.85 && info.seconds < 1.2, "seconds {}", info.seconds);

        let names = stills(&path);
        assert_eq!(names.len() as u64, info.frames, "{names:?}");
        assert_eq!(names[0], "00000000.jpg");
        assert!(names.iter().all(|n| n.len() == 12 && n.ends_with(".jpg") && n[..8].bytes().all(|b| b.is_ascii_digit())));
        // The eight static ticks wrote nothing: the second still is the
        // first change, about 9 ticks of 50 ms in.
        let second: u64 = names[1][..8].parse().unwrap();
        assert!(second >= 350, "{names:?}");
        let grabbed = calls.load(Ordering::SeqCst) as u64;
        // grabbed - 8 static (call 0 wrote the first) + the closing copy.
        assert!(info.frames <= grabbed - 8 + 1, "{} stills from {grabbed} grabs", info.frames);
        assert!(info.frames >= 5);
        // The closing still is at the stop time.
        let last: u64 = names.last().unwrap()[..8].parse().unwrap();
        assert!((last as f32 / 1000.0 - info.seconds).abs() < 0.01);

        // What the MCP side reads back.
        let (w, h, _) = kmscua_mcp::stills::probe(&path).unwrap();
        assert_eq!((w, h), (64, 48));
        // stop() again is harmless.
        assert_eq!(rec.stop().frames, info.frames);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn stops_at_max_seconds() {
        let dir = tmp("max");
        let calls = Arc::new(AtomicU32::new(0));
        let grab = fake(32, 32, calls, |n| (n % 200) as u8);
        let opts = RecordOpts { name: Some("max".into()), fps: Some(20), max_seconds: Some(1), ..Default::default() };
        let mut rec = Recorder::start(&dir, opts, grab).unwrap();
        std::thread::sleep(Duration::from_millis(1400));
        let info = rec.info();
        assert!(!info.recording);
        assert_eq!(info.finalized, Some(true));
        assert!((info.seconds - 1.0).abs() < 1e-3, "seconds {}", info.seconds);
        let names = stills(&dir.join("max.frames"));
        let last: u64 = names.last().unwrap()[..8].parse().unwrap();
        assert!(last <= 1000, "{names:?}");
        assert_eq!(rec.stop().frames, info.frames);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn downscales_and_counts_dropped() {
        let dir = tmp("slow");
        let calls = Arc::new(AtomicU32::new(0));
        let mut inner = fake(200, 100, calls, |n| (n * 30 % 250) as u8);
        // Grabs take 130 ms against a 50 ms tick.
        let grab: Grab = Box::new(move |c| {
            std::thread::sleep(Duration::from_millis(130));
            inner(c)
        });
        let opts = RecordOpts { name: Some("slow".into()), fps: Some(20), max_side: Some(64), ..Default::default() };
        let mut rec = Recorder::start(&dir, opts, grab).unwrap();
        std::thread::sleep(Duration::from_millis(800));
        let info = rec.stop();
        assert_eq!((info.width, info.height), (50, 25));
        assert!(info.dropped >= 4, "dropped {}", info.dropped);
        let first = image::open(dir.join("slow.frames/00000000.jpg")).unwrap();
        assert_eq!((first.width(), first.height()), (50, 25));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn replaces_an_earlier_recording_and_fails_without_a_first_frame() {
        let dir = tmp("again");
        let stale = dir.join("again.frames");
        std::fs::create_dir_all(&stale).unwrap();
        std::fs::write(stale.join("99999999.jpg"), b"old").unwrap();
        std::fs::write(stale.join("notes.txt"), b"keep").unwrap();
        let grab = fake(16, 16, Arc::new(AtomicU32::new(0)), |_| 7);
        let mut rec = Recorder::start(&dir, RecordOpts { name: Some("again".into()), ..Default::default() }, grab).unwrap();
        assert_eq!(rec.info().fps, DEFAULT_FPS);
        rec.stop();
        let names = stills(&stale);
        assert!(!names.contains(&"99999999.jpg".to_string()));
        assert!(names.contains(&"notes.txt".to_string()));

        let broken: Grab = Box::new(|_| anyhow::bail!("no display"));
        assert!(Recorder::start(&dir, RecordOpts::default(), broken).is_err());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn factor_fits_the_longest_side() {
        assert_eq!(plan_factor(3840, 2160, 1920), 2);
        assert_eq!(plan_factor(1920, 1080, 1920), 1);
        assert_eq!(plan_factor(2560, 1440, 1920), 2);
        assert_eq!(plan_factor(1024, 768, 300), 4);
    }
}
