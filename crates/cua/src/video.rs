//! Turning a recording into things a model can look at: contact sheets,
//! frames at times, scene-change times. All through `ffmpeg`/`ffprobe`,
//! which decode H.264 fine on the CPU for this purpose.

use std::io::Write;
use std::path::Path;
use std::process::{Command, Stdio};

use anyhow::{anyhow, bail, Context, Result};

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

/// One PNG per requested time, scaled so the longest side is `max_side`.
pub fn frames_at(path: &Path, times: &[f32], max_side: u32) -> Result<Vec<(f32, Vec<u8>)>> {
    // A seek at or past the last frame's PTS yields nothing; pull the end in.
    let last = (duration(path)? - 0.1).max(0.0);
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
                &format!("scale='if(gt(iw,ih),{max_side},-2)':'if(gt(iw,ih),-2,{max_side})'"),
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

/// Times (seconds) where the picture changed by at least `threshold` (0..1).
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

fn timeline_path(rec: &Path) -> std::path::PathBuf {
    rec.with_extension("timeline.jsonl")
}

pub fn write_timeline(rec: &Path, entries: &[(f32, String)]) -> Result<()> {
    let mut f = std::fs::File::create(timeline_path(rec))?;
    for (i, (t, what)) in entries.iter().enumerate() {
        writeln!(f, "{}", serde_json::json!({"i": i, "t": t, "what": what}))?;
    }
    Ok(())
}

pub fn read_timeline(rec: &Path) -> Result<Vec<(f32, String)>> {
    let text = std::fs::read_to_string(timeline_path(rec))
        .with_context(|| format!("no timeline beside {}", rec.display()))?;
    let mut v = Vec::new();
    for line in text.lines() {
        let j: serde_json::Value = serde_json::from_str(line)?;
        v.push((j["t"].as_f64().unwrap_or(0.0) as f32, j["what"].as_str().unwrap_or("").to_string()));
    }
    Ok(v)
}
