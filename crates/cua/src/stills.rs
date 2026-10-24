//! Recordings stored as a directory of JPEG stills instead of an MP4, for a
//! backend that encodes no video. [`crate::video`] sends a path here when it
//! is a directory, so `record_stop`, `record_frames` and `cua record …`
//! treat both kinds the same.
//!
//! The contract, for whoever writes the directory:
//!
//! - `RecordInfo::path` in the `RecordStart`/`RecordStop` replies names the
//!   directory. Call it `<name>.frames`: the manifest goes beside it at
//!   `video::manifest_path(dir)`, which is `<name>.json`, written by the MCP
//!   layer at `record_stop` exactly as for an MP4.
//! - Each still is `<ms>.jpg`, `ms` the integer milliseconds since the
//!   recording started, zero-padded to 8 digits so names sort by time
//!   (`00001234.jpg`). The first still is `00000000.jpg`. Other files are
//!   ignored.
//! - A still holds until the next one, so the writer may skip frames that
//!   did not change. Every still is `RecordInfo::width` x `height`, the
//!   recording's own pixels, with `scale` video pixels per source pixel as
//!   for an MP4.
//! - `RecordStop` answers once the last still is on disk, with
//!   `finalized: Some(true)`, `seconds`, `fps` (the capture rate) and
//!   `frames` (stills written).
//!
//! Listing, `probe` and `duration` are implemented. `frames_at` and
//! `changes` are not yet; they must return what their `video` namesakes
//! return (PNG per time; runs of changed stills compared at ~320 px grey),
//! with "frame" meaning a still.

use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};

use crate::video::{self, Crop, Run};

/// The stills in `dir`, `(seconds, path)`, in time order.
pub fn list(dir: &Path) -> Result<Vec<(f32, PathBuf)>> {
    let mut v = Vec::new();
    for e in std::fs::read_dir(dir).with_context(|| format!("read {}", dir.display()))? {
        let p = e?.path();
        if p.extension().and_then(|e| e.to_str()) != Some("jpg") {
            continue;
        }
        if let Some(ms) = p.file_stem().and_then(|s| s.to_str()).and_then(|s| s.parse::<u64>().ok()) {
            v.push((ms as f32 / 1000.0, p));
        }
    }
    if v.is_empty() {
        bail!("no <ms>.jpg stills in {}", dir.display());
    }
    v.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap());
    Ok(v)
}

/// Width, height and capture rate: the size of the first still, the rate
/// from the manifest, else from the stills' spacing.
pub fn probe(dir: &Path) -> Result<(u32, u32, f32)> {
    let stills = list(dir)?;
    let (w, h) = image::image_dimensions(&stills[0].1).with_context(|| format!("read {}", stills[0].1.display()))?;
    let fps = match video::read_manifest(dir) {
        Ok(m) if m.fps > 0 => m.fps as f32,
        _ => {
            let span = stills.last().map(|s| s.0).unwrap_or(0.0);
            if span > 0.0 { (stills.len() - 1) as f32 / span } else { 1.0 }
        }
    };
    Ok((w, h, fps))
}

/// Seconds covered: the manifest's duration, else the last still's time.
pub fn duration(dir: &Path) -> Result<f32> {
    if let Ok(m) = video::read_manifest(dir) {
        if m.seconds > 0.0 {
            return Ok(m.seconds);
        }
    }
    Ok(list(dir)?.last().map(|s| s.0).unwrap_or(0.0))
}

/// The still showing at each time, as `video::frames_at` returns it.
pub fn frames_at(dir: &Path, _times: &[f32], _max_side: u32, _crop: Option<Crop>) -> Result<Vec<(f32, Vec<u8>)>> {
    bail!("frames from a stills recording ({}) are not implemented yet", dir.display())
}

/// Runs of changed stills, as `video::changes` returns them.
pub fn changes(dir: &Path, _from: f32, _to: f32, _crop: Option<Crop>, _threshold: f32) -> Result<(Vec<Run>, u32)> {
    bail!("change detection on a stills recording ({}) is not implemented yet", dir.display())
}
