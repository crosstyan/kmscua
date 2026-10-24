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
//! Reading follows from "a still holds until the next one": the frame at
//! time `t` is the latest still at or before `t`, and [`changes`] compares
//! consecutive stills where `video::changes` compares consecutive frames.
//! Both return what their `video` namesakes return, all in pure Rust.

use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use image::{imageops, GrayImage, ImageEncoder, RgbImage};

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

/// Half a millisecond: still times are whole milliseconds read back as f32.
const EPS: f32 = 0.0005;

/// Index of the still showing at `t`: the latest at or before it, else the first.
fn showing(stills: &[(f32, PathBuf)], t: f32) -> usize {
    stills.partition_point(|s| s.0 <= t + EPS).saturating_sub(1)
}

fn open(p: &Path) -> Result<RgbImage> {
    Ok(image::open(p).with_context(|| format!("read {}", p.display()))?.into_rgb8())
}

/// The still showing at each time, as `video::frames_at` returns it: one PNG
/// per time (clamped into the recording), cropped to `crop` if given,
/// scaled so the longest side is `max_side`.
pub fn frames_at(dir: &Path, times: &[f32], max_side: u32, crop: Option<Crop>) -> Result<Vec<(f32, Vec<u8>)>> {
    let stills = list(dir)?;
    let end = duration(dir)?.max(stills.last().map(|s| s.0).unwrap_or(0.0));
    let mut cache: Option<(usize, RgbImage)> = None;
    let mut out = Vec::with_capacity(times.len());
    for &t in times {
        let t = t.clamp(0.0, end);
        let i = showing(&stills, t);
        if cache.as_ref().map(|c| c.0) != Some(i) {
            cache = Some((i, open(&stills[i].1)?));
        }
        let img = &cache.as_ref().unwrap().1;
        out.push((t, render(img, max_side, crop)?));
    }
    Ok(out)
}

/// Crop, then scale the longest side to `max_side` (up or down) and the
/// other to the nearest even size, as ffmpeg's `scale=…:-2` does.
fn render(img: &RgbImage, max_side: u32, crop: Option<Crop>) -> Result<Vec<u8>> {
    let cropped;
    let img = match crop.and_then(|c| c.clamp_to(img.width(), img.height())) {
        Some(c) => {
            cropped = imageops::crop_imm(img, c.x, c.y, c.w, c.h).to_image();
            &cropped
        }
        None => img,
    };
    let (w, h) = img.dimensions();
    let side = max_side.max(2);
    let even = |a: u32, b: u32| (((a as f32 * side as f32 / b as f32) / 2.0).round() as u32 * 2).max(2);
    let (nw, nh) = if w > h { (side, even(h, w)) } else { (even(w, h), side) };
    let scaled;
    let img = if (nw, nh) == (w, h) {
        img
    } else {
        scaled = imageops::resize(img, nw, nh, imageops::FilterType::Triangle);
        &scaled
    };
    let mut png = Vec::new();
    image::codecs::png::PngEncoder::new_with_quality(&mut png, image::codecs::png::CompressionType::Fast, image::codecs::png::FilterType::Sub)
        .write_image(img, img.width(), img.height(), image::ExtendedColorType::Rgb8)?;
    Ok(png)
}

/// Runs of changed stills, as `video::changes` returns them: every still
/// from the one showing at `from` to the last at or before `to` is cropped,
/// scaled to ~320 px wide in grey, and counts as changed when more than
/// `threshold` (0..1) of its pixels moved by more than 24 levels from the
/// still before. A run is consecutive changed stills with no more than a
/// tick and a half between them; a longer gap means the writer skipped
/// unchanged frames there, which ends the run as an unchanged frame would.
/// Returns the runs and the number of stills examined.
pub fn changes(dir: &Path, from: f32, to: f32, crop: Option<Crop>, threshold: f32) -> Result<(Vec<Run>, u32)> {
    let stills = list(dir)?;
    let (vw, vh, fps) = probe(dir)?;
    let first = showing(&stills, from);
    let last = stills.partition_point(|s| s.0 <= to + EPS).max(first + 1);
    let window = &stills[first..last];
    let crop = crop.and_then(|c| c.clamp_to(vw, vh));
    let (cw, ch) = crop.map(|c| (c.w, c.h)).unwrap_or((vw, vh));
    let w = 320u32.min(cw);
    let h = ((ch as f32 * w as f32 / cw as f32).round() as u32).max(1);
    let greys = grey_all(window, crop, w, h)?;
    let frame = (w * h) as usize;
    let gap = 1.5 / fps.max(0.01);
    let mut runs: Vec<Run> = Vec::new();
    let mut open: Option<Run> = None;
    for i in 1..greys.len() {
        let (a, b) = (greys[i - 1].as_raw(), greys[i].as_raw());
        let moved = a.iter().zip(b).filter(|(x, y)| x.abs_diff(**y) > 24).count();
        let frac = moved as f32 / frame as f32;
        let t = window[i].0;
        let adjacent = t - window[i - 1].0 <= gap;
        if frac >= threshold {
            match open.as_mut() {
                Some(r) if adjacent => {
                    r.end = t;
                    r.frames += 1;
                    r.peak = r.peak.max(frac);
                }
                _ => {
                    runs.extend(open.take());
                    open = Some(Run { start: t, end: t, frames: 1, peak: frac });
                }
            }
        } else {
            runs.extend(open.take());
        }
    }
    runs.extend(open);
    Ok((runs, greys.len() as u32))
}

/// Decode, crop and shrink every still to `w`x`h` grey, on all cores.
fn grey_all(stills: &[(f32, PathBuf)], crop: Option<Crop>, w: u32, h: u32) -> Result<Vec<GrayImage>> {
    let grey = |p: &Path| -> Result<GrayImage> {
        let img = image::open(p).with_context(|| format!("read {}", p.display()))?.into_luma8();
        let img = match crop {
            Some(c) => imageops::crop_imm(&img, c.x, c.y, c.w, c.h).to_image(),
            None => img,
        };
        Ok(imageops::resize(&img, w, h, imageops::FilterType::Triangle))
    };
    let threads = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(1).clamp(1, stills.len().max(1));
    let per = stills.len().div_ceil(threads).max(1);
    let parts: Vec<Result<Vec<GrayImage>>> = std::thread::scope(|sc| {
        let handles: Vec<_> = stills
            .chunks(per)
            .map(|chunk| sc.spawn(move || chunk.iter().map(|(_, p)| grey(p)).collect::<Result<Vec<_>>>()))
            .collect();
        handles.into_iter().map(|h| h.join().unwrap_or_else(|_| bail!("decoder thread panicked"))).collect()
    });
    let mut out = Vec::with_capacity(stills.len());
    for p in parts {
        out.extend(p?);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `(ms, grey level, white 20 px box at (x, y))`.
    type Still = (u64, u8, Option<(u32, u32)>);

    /// A recording directory of 200x100 flat-colour stills.
    fn recording(tag: &str, stills: &[Still]) -> PathBuf {
        let root = std::env::temp_dir().join(format!("cua-stills-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let dir = root.join("rec.frames");
        std::fs::create_dir_all(&dir).unwrap();
        for &(ms, v, bx) in stills {
            let mut img = RgbImage::from_pixel(200, 100, image::Rgb([v, v, v]));
            if let Some((x, y)) = bx {
                for yy in y..y + 20 {
                    for xx in x..x + 20 {
                        img.put_pixel(xx, yy, image::Rgb([255, 255, 255]));
                    }
                }
            }
            img.save(dir.join(format!("{ms:08}.jpg"))).unwrap();
        }
        std::fs::write(dir.join("notes.txt"), b"ignored").unwrap();
        dir
    }

    fn grey_at(png: &[u8], x: u32, y: u32) -> u8 {
        image::load_from_memory(png).unwrap().to_luma8().get_pixel(x, y).0[0]
    }

    #[test]
    fn probe_and_duration_without_a_manifest() {
        let dir = recording("probe", &[(0, 10, None), (200, 10, None), (400, 10, None)]);
        assert_eq!(list(&dir).unwrap().len(), 3);
        let (w, h, fps) = probe(&dir).unwrap();
        assert_eq!((w, h), (200, 100));
        assert!((fps - 5.0).abs() < 1e-3);
        assert!((duration(&dir).unwrap() - 0.4).abs() < 1e-4);
        // The manifest wins when present.
        let m = video::Manifest { fps: 10, seconds: 3.5, ..Default::default() };
        video::write_manifest(&dir, &m).unwrap();
        assert_eq!(probe(&dir).unwrap().2, 10.0);
        assert_eq!(duration(&dir).unwrap(), 3.5);
        std::fs::remove_dir_all(dir.parent().unwrap()).unwrap();
    }

    #[test]
    fn frame_at_is_the_latest_still_at_or_before() {
        let dir = recording("at", &[(0, 20, None), (1000, 120, None), (2500, 220, None)]);
        let f = frames_at(&dir, &[0.0, 0.999, 1.0, 2.4, 2.5, 9.0, -1.0], 100, None).unwrap();
        let t: Vec<f32> = f.iter().map(|x| x.0).collect();
        assert_eq!(t, vec![0.0, 0.999, 1.0, 2.4, 2.5, 2.5, 0.0]);
        let g: Vec<u8> = f.iter().map(|x| grey_at(&x.1, 10, 10)).collect();
        let near = |a: u8, b: u8| a.abs_diff(b) <= 4;
        assert!(near(g[0], 20) && near(g[1], 20) && near(g[2], 120) && near(g[3], 120) && near(g[4], 220) && near(g[5], 220) && near(g[6], 20), "{g:?}");
        // Longest side to max_side, the other even.
        let img = image::load_from_memory(&f[0].1).unwrap();
        assert_eq!((img.width(), img.height()), (100, 50));
        std::fs::remove_dir_all(dir.parent().unwrap()).unwrap();
    }

    #[test]
    fn frames_at_crops_in_recording_pixels() {
        let dir = recording("crop", &[(0, 0, Some((150, 60)))]);
        let crop = Crop { x: 140, y: 50, w: 40, h: 40 };
        let f = frames_at(&dir, &[0.0], 80, Some(crop)).unwrap();
        let img = image::load_from_memory(&f[0].1).unwrap();
        assert_eq!((img.width(), img.height()), (80, 80));
        // The box sits at 10..30 of the crop, scaled 2x: 20..60.
        assert!(grey_at(&f[0].1, 40, 40) > 230);
        assert!(grey_at(&f[0].1, 5, 5) < 25);
        std::fs::remove_dir_all(dir.parent().unwrap()).unwrap();
    }

    #[test]
    fn changes_are_runs_of_changed_stills() {
        // At 5 fps (200 ms ticks): a run of two changes at 0.4-0.6 s, a
        // static gap, a lone change at 2.0 s, and a closing copy at 3.0 s.
        let dir = recording(
            "changes",
            &[(0, 30, None), (200, 30, Some((0, 0))), (400, 200, None), (600, 60, None), (2000, 140, None), (3000, 140, None)],
        );
        video::write_manifest(&dir, &video::Manifest { fps: 5, seconds: 3.0, ..Default::default() }).unwrap();
        let (runs, n) = changes(&dir, 0.0, 3.0, None, 0.5).unwrap();
        assert_eq!(n, 6);
        let got: Vec<(f32, f32, u32)> = runs.iter().map(|r| (r.start, r.end, r.frames)).collect();
        assert_eq!(got, vec![(0.4, 0.6, 2), (2.0, 2.0, 1)]);
        assert!(runs.iter().all(|r| r.peak > 0.9));

        // A low threshold also catches the small box at 0.2 s, which joins
        // the run after it.
        let (runs, _) = changes(&dir, 0.0, 3.0, None, 0.01).unwrap();
        assert_eq!(runs[0].start, 0.2);
        assert_eq!(runs[0].frames, 3);

        // A window: the baseline is the still showing at `from`.
        let (runs, n) = changes(&dir, 1.0, 2.5, None, 0.5).unwrap();
        assert_eq!(n, 2);
        assert_eq!(runs.len(), 1);
        assert_eq!(runs[0].start, 2.0);

        // A crop around the box sees only the box.
        let crop = Crop { x: 0, y: 0, w: 40, h: 40 };
        let (runs, _) = changes(&dir, 0.0, 0.3, Some(crop), 0.1).unwrap();
        assert_eq!(runs.len(), 1);
        assert_eq!(runs[0].start, 0.2);
        assert!(runs[0].peak > 0.2);
        std::fs::remove_dir_all(dir.parent().unwrap()).unwrap();
    }

    #[test]
    fn video_dispatches_directories_here() {
        let dir = recording("dispatch", &[(0, 10, None), (500, 200, None)]);
        assert_eq!(video::frames_at(&dir, &[0.6], 64, None).unwrap().len(), 1);
        assert_eq!(video::changes(&dir, 0.0, 1.0, None, 0.5).unwrap().1, 2);
        let (png, times) = video::contact_sheet(&dir, 0.5, 2, 64).unwrap();
        assert!(!png.is_empty() && times.len() == 2);
        std::fs::remove_dir_all(dir.parent().unwrap()).unwrap();
    }
}
