//! Turning a recording into things a model can look at: contact sheets,
//! frames at times, scene-change times. All through `ffmpeg`/`ffprobe`,
//! which decode H.264 fine on the CPU for this purpose.

use std::io::Write;
use std::path::Path;
use std::process::{Command, Stdio};

use anyhow::{anyhow, bail, Context, Result};
use serde::{Deserialize, Serialize};

fn run(cmd: &mut Command) -> Result<Vec<u8>> {
    let out = cmd.stdin(Stdio::null()).output().with_context(|| format!("run {:?}", cmd.get_program()))?;
    if !out.status.success() {
        let tail: String = String::from_utf8_lossy(&out.stderr)
            .lines()
            .rev()
            .take(4)
            .collect::<Vec<_>>()
            .join(" | ");
        bail!("{:?} failed: {tail}", cmd.get_program());
    }
    Ok(out.stdout)
}

pub fn have_ffmpeg() -> bool {
    Command::new("ffmpeg").arg("-version").stdout(Stdio::null()).stderr(Stdio::null()).status().map(|s| s.success()).unwrap_or(false)
}

/// Pixel rectangle in the recording's own pixels.
#[derive(Clone, Copy, Debug)]
pub struct Crop {
    pub x: u32,
    pub y: u32,
    pub w: u32,
    pub h: u32,
}

impl Crop {
    fn vf(&self) -> String {
        format!("crop={}:{}:{}:{},", self.w, self.h, self.x, self.y)
    }
    /// Clamp into a `w`x`h` frame; None if nothing is left.
    pub fn clamp_to(self, w: u32, h: u32) -> Option<Crop> {
        let x = self.x.min(w.saturating_sub(1));
        let y = self.y.min(h.saturating_sub(1));
        let cw = self.w.min(w - x);
        let ch = self.h.min(h - y);
        (cw >= 2 && ch >= 2).then_some(Crop { x, y, w: cw, h: ch })
    }
}

/// Width, height and frames per second of the video stream.
pub fn probe(path: &Path) -> Result<(u32, u32, f32)> {
    let out = run(Command::new("ffprobe").args([
        "-v",
        "error",
        "-select_streams",
        "v:0",
        "-show_entries",
        "stream=width,height,r_frame_rate",
        "-of",
        "default=nw=1:nk=1",
    ])
    .arg(path))?;
    let text = String::from_utf8_lossy(&out);
    let mut it = text.lines();
    let w: u32 = it.next().unwrap_or("0").trim().parse().unwrap_or(0);
    let h: u32 = it.next().unwrap_or("0").trim().parse().unwrap_or(0);
    let fps = match it.next().unwrap_or("30/1").trim().split_once('/') {
        Some((n, d)) => n.parse::<f32>().unwrap_or(30.0) / d.parse::<f32>().unwrap_or(1.0).max(1.0),
        None => 30.0,
    };
    if w == 0 || h == 0 {
        bail!("ffprobe found no video stream in {}", path.display());
    }
    Ok((w, h, fps))
}

pub fn duration(path: &Path) -> Result<f32> {
    let out = run(Command::new("ffprobe").args([
        "-v",
        "error",
        "-show_entries",
        "format=duration",
        "-of",
        "default=nw=1:nk=1",
    ])
    .arg(path))?;
    String::from_utf8_lossy(&out).trim().parse::<f32>().map_err(|e| anyhow!("parse duration: {e}"))
}

/// Evenly spaced tiles (first and last frame included) in one PNG.
/// Returns the PNG and the timestamps of the tiles in reading order.
pub fn contact_sheet(path: &Path, dur: f32, tiles: u32, tile_w: u32) -> Result<(Vec<u8>, Vec<f32>)> {
    let tiles = tiles.max(1);
    let cols = match tiles {
        1 => 1,
        2 | 4 => 2,
        3 | 5 | 6 | 9 => 3,
        _ => 4,
    };
    let rows = (tiles + cols - 1) / cols;
    let times: Vec<f32> = (0..tiles)
        .map(|i| if tiles == 1 { 0.0 } else { (dur - 0.05).max(0.0) * i as f32 / (tiles - 1) as f32 })
        .collect();
    // select by nearest frame to each time: build a select expression.
    let select = times
        .iter()
        .map(|t| format!("lt(abs(t-{t:.3}),0.034)"))
        .collect::<Vec<_>>()
        .join("+");
    let vf = format!(
        "select='{select}',scale={tile_w}:-2,tile={cols}x{rows}:padding=4:margin=4:color=0x202020",
    );
    let out = run(Command::new("ffmpeg")
        .args(["-v", "error", "-y", "-i"])
        .arg(path)
        .args(["-vf", &vf, "-vsync", "vfr", "-frames:v", "1", "-f", "image2pipe", "-vcodec", "png", "-"]))?;
    if out.is_empty() {
        bail!("ffmpeg produced no sheet");
    }
    Ok((out, times))
}

/// One PNG per requested time, cropped to `crop` if given, scaled so the
/// longest side is `max_side`.
pub fn frames_at(path: &Path, times: &[f32], max_side: u32, crop: Option<Crop>) -> Result<Vec<(f32, Vec<u8>)>> {
    // A seek at or past the last frame's PTS yields nothing; pull the end in.
    let last = (duration(path)? - 0.1).max(0.0);
    let crop_vf = crop.map(|c| c.vf()).unwrap_or_default();
    let mut out = Vec::with_capacity(times.len());
    for &t in times {
        let t = t.clamp(0.0, last);
        let png = run(Command::new("ffmpeg")
            .args(["-v", "error", "-y", "-ss", &format!("{t:.3}"), "-i"])
            .arg(path)
            .args([
                "-frames:v",
                "1",
                "-vf",
                &format!("{crop_vf}scale='if(gt(iw,ih),{max_side},-2)':'if(gt(iw,ih),-2,{max_side})'"),
                "-f",
                "image2pipe",
                "-vcodec",
                "png",
                "-",
            ]))?;
        if !png.is_empty() {
            out.push((t, png));
        }
    }
    Ok(out)
}

/// A run of consecutive frames that each differ from their predecessor.
#[derive(Clone, Copy, Debug)]
pub struct Run {
    pub start: f32,
    pub end: f32,
    pub frames: u32,
    /// Largest fraction of changed pixels seen in the run.
    pub peak: f32,
}

/// Per-frame pixel diff inside `crop` between `from` and `to`: every frame is
/// decoded at ~320 px wide in grey, and a frame counts as changed when more
/// than `threshold` (0..1) of its pixels moved by more than 24 levels from
/// the frame before. Returns the runs of changed frames and the total frame
/// count examined. Exact for UI where ffmpeg's scene score is not.
pub fn changes(path: &Path, from: f32, to: f32, crop: Option<Crop>, threshold: f32) -> Result<(Vec<Run>, u32)> {
    let (vw, vh, fps) = probe(path)?;
    let (cw, ch) = crop.map(|c| (c.w, c.h)).unwrap_or((vw, vh));
    let w = 320u32.min(cw);
    let h = ((ch as f32 * w as f32 / cw as f32).round() as u32).max(1);
    let vf = format!("{}scale={w}:{h}", crop.map(|c| c.vf()).unwrap_or_default());
    let raw = run(Command::new("ffmpeg")
        .args(["-v", "error", "-ss", &format!("{from:.3}"), "-i"])
        .arg(path)
        .args(["-t", &format!("{:.3}", (to - from).max(0.0)), "-vf", &vf, "-vsync", "0", "-f", "rawvideo", "-pix_fmt", "gray", "-"]))?;
    let frame = (w * h) as usize;
    let n = raw.len() / frame.max(1);
    let mut runs: Vec<Run> = Vec::new();
    let mut open: Option<Run> = None;
    for i in 1..n {
        let a = &raw[(i - 1) * frame..i * frame];
        let b = &raw[i * frame..(i + 1) * frame];
        let moved = a.iter().zip(b).filter(|(x, y)| x.abs_diff(**y) > 24).count();
        let frac = moved as f32 / frame as f32;
        let t = from + i as f32 / fps;
        if frac >= threshold {
            match open.as_mut() {
                Some(r) => {
                    r.end = t;
                    r.frames += 1;
                    r.peak = r.peak.max(frac);
                }
                None => open = Some(Run { start: t, end: t, frames: 1, peak: frac }),
            }
        } else if let Some(r) = open.take() {
            runs.push(r);
        }
    }
    if let Some(r) = open {
        runs.push(r);
    }
    Ok((runs, n as u32))
}

/// Tile PNGs into one grid, `cols` across, reading order, dark gutters.
pub fn tile(pngs: &[Vec<u8>], cols: u32) -> Result<Vec<u8>> {
    use image::{imageops, ImageEncoder, RgbaImage};
    let imgs: Vec<RgbaImage> = pngs.iter().map(|p| Ok(image::load_from_memory(p)?.to_rgba8())).collect::<Result<_>>()?;
    let cols = cols.max(1).min(imgs.len().max(1) as u32);
    let rows = (imgs.len() as u32 + cols - 1) / cols;
    let tw = imgs.iter().map(|i| i.width()).max().unwrap_or(1);
    let th = imgs.iter().map(|i| i.height()).max().unwrap_or(1);
    let pad = 4;
    let mut sheet = RgbaImage::from_pixel(cols * (tw + pad) + pad, rows * (th + pad) + pad, image::Rgba([0x20, 0x20, 0x20, 0xff]));
    for (i, im) in imgs.iter().enumerate() {
        let (c, r) = (i as u32 % cols, i as u32 / cols);
        imageops::overlay(&mut sheet, im, (pad + c * (tw + pad)) as i64, (pad + r * (th + pad)) as i64);
    }
    let mut out = Vec::new();
    image::codecs::png::PngEncoder::new_with_quality(&mut out, image::codecs::png::CompressionType::Fast, image::codecs::png::FilterType::Sub)
        .write_image(&sheet, sheet.width(), sheet.height(), image::ExtendedColorType::Rgba8)?;
    Ok(out)
}

/// Times (seconds) where the picture changed by at least `threshold` (0..1),
/// by ffmpeg's whole-frame scene score. Coarse on UI; see `changes`.
#[allow(dead_code)]
pub fn scene_changes(path: &Path, threshold: f32, from: f32, to: f32) -> Result<Vec<f32>> {
    let out = Command::new("ffmpeg")
        .args(["-v", "info", "-ss", &format!("{from:.3}"), "-to", &format!("{to:.3}"), "-i"])
        .arg(path)
        .args(["-vf", &format!("select='gt(scene,{threshold})',showinfo"), "-f", "null", "-"])
        .stdin(Stdio::null())
        .output()
        .context("run ffmpeg scene detection")?;
    let text = String::from_utf8_lossy(&out.stderr);
    let mut times = vec![from];
    for line in text.lines() {
        if let Some(i) = line.find("pts_time:") {
            let rest = &line[i + 9..];
            let tok: String = rest.chars().take_while(|c| c.is_ascii_digit() || *c == '.').collect();
            if let Ok(t) = tok.parse::<f32>() {
                times.push(from + t);
            }
        }
    }
    times.push(to);
    times.dedup_by(|a, b| (*a - *b).abs() < 0.05);
    Ok(times)
}

/// Keep at most `max` times, always the first and last.
pub fn thin(mut times: Vec<f32>, max: usize) -> Vec<f32> {
    times.sort_by(|a, b| a.partial_cmp(b).unwrap());
    times.dedup_by(|a, b| (*a - *b).abs() < 0.02);
    if times.len() <= max || max < 2 {
        times.truncate(max.max(1));
        return times;
    }
    let n = times.len();
    (0..max).map(|i| times[i * (n - 1) / (max - 1)]).collect()
}

/// Sidecar `<recording>.json`: everything about the file that the video
/// itself does not carry. Wall-clock times let a recording be lined up with
/// application logs; the source size maps screen coordinates to video pixels.
#[derive(Serialize, Deserialize, Clone, Debug, Default)]
pub struct Manifest {
    pub path: String,
    /// Local time the recording started, RFC 3339 with milliseconds.
    pub started_at: String,
    pub started_unix_ms: u64,
    pub stopped_unix_ms: u64,
    pub seconds: f32,
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    pub frames: u64,
    pub dropped: u64,
    pub codec: String,
    pub finalized: bool,
    /// Scanout size the frames were scaled from.
    pub source_width: u32,
    pub source_height: u32,
    /// Video pixels per scanout pixel.
    pub scale: f32,
    pub timeline: Vec<TimelineEntry>,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct TimelineEntry {
    pub i: usize,
    /// Seconds into the recording.
    pub t: f32,
    pub unix_ms: u64,
    pub what: String,
}

pub fn unix_ms(t: std::time::SystemTime) -> u64 {
    t.duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0)
}

pub fn rfc3339_local(t: std::time::SystemTime) -> String {
    chrono::DateTime::<chrono::Local>::from(t).to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
}

pub fn manifest_path(rec: &Path) -> std::path::PathBuf {
    rec.with_extension("json")
}

pub fn write_manifest(rec: &Path, m: &Manifest) -> Result<()> {
    let mut f = std::fs::File::create(manifest_path(rec))?;
    writeln!(f, "{}", serde_json::to_string_pretty(m)?)?;
    Ok(())
}

pub fn read_manifest(rec: &Path) -> Result<Manifest> {
    let text = std::fs::read_to_string(manifest_path(rec)).with_context(|| format!("no manifest beside {}", rec.display()))?;
    Ok(serde_json::from_str(&text)?)
}

/// Timeline from the manifest, or from the older `<rec>.timeline.jsonl`.
pub fn read_timeline(rec: &Path) -> Result<Vec<(f32, String)>> {
    if let Ok(m) = read_manifest(rec) {
        return Ok(m.timeline.into_iter().map(|e| (e.t, e.what)).collect());
    }
    let text = std::fs::read_to_string(rec.with_extension("timeline.jsonl"))
        .with_context(|| format!("no manifest or timeline beside {}", rec.display()))?;
    let mut v = Vec::new();
    for line in text.lines() {
        let j: serde_json::Value = serde_json::from_str(line)?;
        v.push((j["t"].as_f64().unwrap_or(0.0) as f32, j["what"].as_str().unwrap_or("").to_string()));
    }
    Ok(v)
}

/// Screen rectangle (scanout pixels) to a crop in the recording's pixels.
pub fn crop_from_scanout(m: &Manifest, x: i32, y: i32, w: i32, h: i32) -> Option<Crop> {
    let s = if m.scale > 0.0 { m.scale } else { 1.0 };
    let f = |v: i32| (v as f32 * s).round().max(0.0) as u32;
    Crop { x: f(x), y: f(y), w: f(w).max(2), h: f(h).max(2) }.clamp_to(m.width, m.height)
}

/// Build a manifest from what the daemon reports plus the scanout size.
pub fn manifest_from(info: &kmscua_proto::RecordInfo, started: std::time::SystemTime, source: (u32, u32), timeline: &[(f32, String)]) -> Manifest {
    let stopped = started + std::time::Duration::from_secs_f32(info.seconds.max(0.0));
    let s0 = unix_ms(started);
    Manifest {
        path: info.path.clone().unwrap_or_default(),
        started_at: rfc3339_local(started),
        started_unix_ms: s0,
        stopped_unix_ms: unix_ms(stopped),
        seconds: info.seconds,
        width: info.width,
        height: info.height,
        fps: info.fps,
        frames: info.frames,
        dropped: info.dropped,
        codec: info.codec.clone().unwrap_or_default(),
        finalized: info.finalized.unwrap_or(false),
        source_width: source.0,
        source_height: source.1,
        scale: if source.0 > 0 { info.width as f32 / source.0 as f32 } else { 1.0 },
        timeline: timeline
            .iter()
            .enumerate()
            .map(|(i, (t, what))| TimelineEntry { i, t: *t, unix_ms: s0 + (*t * 1000.0) as u64, what: what.clone() })
            .collect(),
    }
}

/// Runs rendered as one line for a model or a terminal.
pub fn describe_runs(runs: &[Run], n: u32, fps: f32, from: f32, to: f32, threshold: f32, where_: &str) -> String {
    if runs.is_empty() {
        return format!("no frame changed more than {threshold} of the {where_} in {n} frames from {from:.2}s to {to:.2}s ({fps:.0} fps)");
    }
    let list: Vec<String> = runs
        .iter()
        .take(40)
        .map(|r| {
            if r.frames == 1 {
                format!("{:.2}s", r.start)
            } else {
                format!("{:.2}-{:.2}s ({} frames)", r.start, r.end, r.frames)
            }
        })
        .collect();
    format!(
        "{} run{} of changed frames in the {where_}, threshold {threshold}, {n} frames at {fps:.0} fps from {from:.2}s to {to:.2}s: {}{}",
        runs.len(),
        if runs.len() == 1 { "" } else { "s" },
        list.join(", "),
        if runs.len() > 40 { ", …" } else { "" }
    )
}

/// Frame before each run, the run start, and the run end when it is long.
pub fn run_times(runs: &[Run], fps: f32, from: f32) -> Vec<f32> {
    let mut v = Vec::new();
    for r in runs {
        v.push((r.start - 1.0 / fps).max(from));
        v.push(r.start);
        if r.frames > 1 {
            v.push(r.end);
        }
    }
    v
}
