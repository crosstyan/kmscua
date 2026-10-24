//! Crop, cursor composite, downscale and encode: cuad's image pipeline.
//!
//! Copied from `crates/cuad/src/capture.rs` (`downscale_to`, `downscale_box`,
//! `clamp_rect`, `composite_cursor` and the tail of `Capturer::screenshot`)
//! rather than shared, because cuad is a binary that links libdrmtap and
//! xcua must not. Keep the two in step: the MCP layer's coordinate mapping
//! relies on `ScreenshotInfo` meaning the same thing from both.

use std::time::Instant;

use anyhow::{Context, Result};
use image::{codecs::jpeg::JpegEncoder, ImageEncoder, RgbaImage};
use kmscua_proto::{CursorInfo, ImageFormat, Rect, ScreenshotInfo};

/// A captured region as tightly packed RGBA plus where it came from.
pub struct Shot {
    pub rgba: RgbaImage,
    pub origin: (i32, i32),
    pub cursor: Option<CursorInfo>,
    pub grab_ms: f32,
}

/// Downscale to `max_side`, encode, and describe the result the way cuad
/// does: `scale` source pixels per image pixel, `origin` the region offset.
pub fn finish(
    shot: Shot,
    max_side: Option<u32>,
    format: ImageFormat,
    quality: Option<u8>,
) -> Result<(ScreenshotInfo, Vec<u8>)> {
    let t0 = Instant::now();
    let (sw, sh) = shot.rgba.dimensions();
    let (img, scale) = match max_side {
        Some(m) if m > 0 && (sw > m || sh > m) => {
            let img = downscale_to(&shot.rgba, m);
            let s = sw as f32 / img.width() as f32;
            (img, s)
        }
        _ => (shot.rgba, 1.0),
    };
    let (w, h) = img.dimensions();
    let buf = encode(&img, format, quality)?;
    let info = ScreenshotInfo {
        width: w,
        height: h,
        source_width: sw,
        source_height: sh,
        scale,
        origin_x: shot.origin.0,
        origin_y: shot.origin.1,
        mime: format.mime().to_string(),
        cursor: shot.cursor,
        grab_ms: shot.grab_ms,
        encode_ms: t0.elapsed().as_secs_f32() * 1000.0,
    };
    Ok((info, buf))
}

/// PNG at the fast zlib level with the Sub filter, or JPEG (default q85).
pub fn encode(img: &RgbaImage, format: ImageFormat, quality: Option<u8>) -> Result<Vec<u8>> {
    let (w, h) = img.dimensions();
    let mut buf = Vec::with_capacity((w * h) as usize);
    match format {
        ImageFormat::Png => {
            let enc = image::codecs::png::PngEncoder::new_with_quality(
                &mut buf,
                image::codecs::png::CompressionType::Fast,
                image::codecs::png::FilterType::Sub,
            );
            enc.write_image(img.as_raw(), w, h, image::ExtendedColorType::Rgba8)
                .context("encode png")?;
        }
        ImageFormat::Jpeg => {
            let rgb = image::DynamicImage::ImageRgba8(img.clone()).into_rgb8();
            let enc = JpegEncoder::new_with_quality(&mut buf, quality.unwrap_or(85).clamp(1, 100));
            enc.write_image(rgb.as_raw(), w, h, image::ExtendedColorType::Rgb8)
                .context("encode jpeg")?;
        }
    }
    Ok(buf)
}

/// Downscale so the longest side equals `max_side` (aspect kept): an
/// integer box filter for the bulk, `fast_image_resize` for the residual.
/// See cuad's copy for the measurements behind the two stages.
pub fn downscale_to(src: &RgbaImage, max_side: u32) -> RgbaImage {
    let (sw, sh) = src.dimensions();
    let longest = sw.max(sh);
    let max_side = max_side.max(16);
    if longest <= max_side {
        return src.clone();
    }
    let factor = longest / max_side; // floor: stays >= max_side
    let coarse = if factor > 1 { downscale_box(src, factor) } else { src.clone() };
    let (cw, ch) = coarse.dimensions();
    if cw.max(ch) <= max_side {
        return coarse;
    }
    let s = max_side as f32 / cw.max(ch) as f32;
    let (dw, dh) = (((cw as f32 * s).round() as u32).max(1), ((ch as f32 * s).round() as u32).max(1));
    use fast_image_resize as fr;
    let src_img = fr::images::ImageRef::new(cw, ch, coarse.as_raw(), fr::PixelType::U8x4).expect("image ref");
    let mut dst = fr::images::Image::new(dw, dh, fr::PixelType::U8x4);
    let opts = fr::ResizeOptions::new().resize_alg(fr::ResizeAlg::Convolution(fr::FilterType::Bilinear));
    fr::Resizer::new().resize(&src_img, &mut dst, &opts).expect("resize");
    RgbaImage::from_raw(dw, dh, dst.into_vec()).expect("sized buffer")
}

/// Box (area-average) downscale by an integer factor, parallel over rows.
pub fn downscale_box(src: &RgbaImage, factor: u32) -> RgbaImage {
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

/// `r` clipped to a `fw` x `fh` frame; None when nothing is left.
pub fn clamp_rect(r: Rect, fw: u32, fh: u32) -> Option<Rect> {
    let x0 = r.x.max(0) as u32;
    let y0 = r.y.max(0) as u32;
    let x1 = (r.x as i64 + r.width as i64).clamp(0, fw as i64) as u32;
    let y1 = (r.y as i64 + r.height as i64).clamp(0, fh as i64) as u32;
    if x1 <= x0 || y1 <= y0 {
        return None;
    }
    Some(Rect {
        x: x0 as i32,
        y: y0 as i32,
        width: x1 - x0,
        height: y1 - y0,
    })
}

/// Premultiplied source-over of the cursor image into the RGBA region.
/// `c.x, c.y` is the image's top-left on screen (pointer minus hotspot),
/// `px` premultiplied ARGB, one u32 per pixel: what XFIXES returns and what
/// the DRM cursor plane holds.
pub fn composite_cursor(img: &mut RgbaImage, region: &Rect, c: &CursorInfo, px: &[u32]) {
    let (w, h) = img.dimensions();
    for row in 0..c.height {
        let y = c.y + row as i32 - region.y;
        if y < 0 || y >= h as i32 {
            continue;
        }
        for col in 0..c.width {
            let x = c.x + col as i32 - region.x;
            if x < 0 || x >= w as i32 {
                continue;
            }
            let Some(&cur) = px.get((row * c.width + col) as usize) else { continue };
            let a = (cur >> 24) & 0xff;
            if a == 0 {
                continue;
            }
            let cb = cur & 0xff;
            let cg = (cur >> 8) & 0xff;
            let cr = (cur >> 16) & 0xff;
            let p = img.get_pixel_mut(x as u32, y as u32);
            let inv = 255 - a;
            p[0] = (cr + p[0] as u32 * inv / 255).min(255) as u8;
            p[1] = (cg + p[1] as u32 * inv / 255).min(255) as u8;
            p[2] = (cb + p[2] as u32 * inv / 255).min(255) as u8;
            p[3] = 0xff;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finish_reports_scale_like_cuad() {
        let shot = Shot { rgba: RgbaImage::new(3840, 2160), origin: (0, 0), cursor: None, grab_ms: 0.0 };
        let (info, png) = finish(shot, Some(1920), ImageFormat::Png, None).unwrap();
        assert_eq!((info.width, info.height), (1920, 1080));
        assert_eq!((info.source_width, info.source_height), (3840, 2160));
        assert_eq!(info.scale, 2.0);
        assert!(!png.is_empty());
    }

    #[test]
    fn clamp_rect_clips() {
        let r = clamp_rect(Rect { x: -10, y: 5, width: 50, height: 2000 }, 100, 100).unwrap();
        assert_eq!((r.x, r.y, r.width, r.height), (0, 5, 40, 95));
        assert!(clamp_rect(Rect { x: 200, y: 0, width: 5, height: 5 }, 100, 100).is_none());
    }
}
