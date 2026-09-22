//! Scanout capture through libdrmtap, with the hardware cursor composited in.
//!
//! The cursor is not in the scanout: it sits on its own KMS plane. A single
//! image for a model therefore has to composite it, which is the one thing
//! this module does beyond "grab and encode". Frame bytes come back from
//! libdrmtap as little-endian BGRX (DRM `XR24`), cursor pixels as premultiplied
//! ARGB8888, both B,G,R,(A) in memory.

use std::time::Instant;

use anyhow::{anyhow, bail, Context, Result};
use image::{codecs::jpeg::JpegEncoder, imageops::FilterType, ImageEncoder, RgbaImage};
use kmscua_proto::{CursorInfo, DisplayInfo, ImageFormat, Rect, ScreenshotInfo};
use libdrmtap::{Config, DrmTap};

const FOURCC_XR24: u32 = 0x3432_5258; // 'XR24' BGRX little-endian
const FOURCC_AR24: u32 = 0x3432_5241; // 'AR24' BGRA
const FOURCC_XB24: u32 = 0x3432_4258; // 'XB24' RGBX
const FOURCC_AB24: u32 = 0x3432_4241; // 'AB24' RGBA

pub struct Capturer {
    tap: DrmTap,
    cursor_ok: bool,
    gpu_driver: Option<String>,
}

/// A captured region as tightly packed RGBA plus where it came from.
pub struct Shot {
    pub rgba: RgbaImage,
    pub origin: (i32, i32),
    pub cursor: Option<CursorInfo>,
    pub grab_ms: f32,
}

impl Capturer {
    pub fn open(device: Option<String>, crtc_id: u32, helper: Option<String>) -> Result<Self> {
        let cfg = Config {
            device_path: device,
            crtc_id,
            helper_path: helper,
            debug: false,
        };
        let mut tap = DrmTap::open(Some(cfg)).map_err(|e| anyhow!("drmtap_open: {e}"))?;
        let gpu_driver = tap.gpu_driver();
        // Pre-warm: the first grab pays for EGL context and detile setup
        // (44 ms measured on the Orin); nobody should pay that on a request.
        let mut cursor_ok = false;
        match tap.grab_mapped() {
            Ok(frame) => {
                log::info!(
                    "pre-warm ok: {}x{} stride={} fourcc={:#x} modifier={:#x}",
                    frame.width(),
                    frame.height(),
                    frame.stride(),
                    frame.format(),
                    frame.modifier()
                );
                drop(frame);
                cursor_ok = tap.get_cursor().is_ok();
            }
            Err(e) => log::warn!("pre-warm grab failed (will retry per request): {e}"),
        }
        Ok(Self {
            tap,
            cursor_ok,
            gpu_driver,
        })
    }

    pub fn gpu_driver(&self) -> Option<String> {
        self.gpu_driver.clone()
    }

    pub fn cursor_supported(&self) -> bool {
        self.cursor_ok
    }

    pub fn displays(&mut self) -> Result<Vec<DisplayInfo>> {
        let list = self
            .tap
            .list_displays()
            .map_err(|e| anyhow!("list_displays: {e}"))?;
        Ok(list
            .into_iter()
            .map(|d| DisplayInfo {
                name: d.name,
                crtc_id: d.crtc_id,
                connector_id: d.connector_id,
                x: d.x,
                y: d.y,
                width: d.width,
                height: d.height,
                refresh_hz: d.refresh_hz,
                active: d.active,
            })
            .collect())
    }

    /// Union of all active displays, in scanout pixels.
    pub fn desktop_rect(&mut self) -> Result<Rect> {
        let displays = self.displays()?;
        let mut x0 = i64::MAX;
        let mut y0 = i64::MAX;
        let mut x1 = 0i64;
        let mut y1 = 0i64;
        for d in displays.iter().filter(|d| d.active) {
            x0 = x0.min(d.x as i64);
            y0 = y0.min(d.y as i64);
            x1 = x1.max(d.x as i64 + d.width as i64);
            y1 = y1.max(d.y as i64 + d.height as i64);
        }
        if x1 == 0 || y1 == 0 {
            bail!("no active display");
        }
        Ok(Rect {
            x: x0 as i32,
            y: y0 as i32,
            width: (x1 - x0) as u32,
            height: (y1 - y0) as u32,
        })
    }

    pub fn cursor(&mut self) -> Result<CursorInfo> {
        let c = self.tap.get_cursor().map_err(|e| anyhow!("get_cursor: {e}"))?;
        Ok(CursorInfo {
            visible: c.visible(),
            x: c.x(),
            y: c.y(),
            hot_x: c.hot_x(),
            hot_y: c.hot_y(),
            width: c.width(),
            height: c.height(),
        })
    }

    /// Grab one frame, optionally crop, composite the cursor.
    pub fn grab(&mut self, with_cursor: bool, region: Option<Rect>) -> Result<Shot> {
        let t0 = Instant::now();
        let frame = self
            .tap
            .grab_mapped()
            .map_err(|e| anyhow!("grab_mapped: {e}"))?;
        let (fw, fh, stride) = (frame.width(), frame.height(), frame.stride() as usize);
        let data = frame
            .data()
            .ok_or_else(|| anyhow!("grab_mapped returned no mapped data"))?;
        let swap_rb = match frame.format() {
            FOURCC_XR24 | FOURCC_AR24 => true,
            FOURCC_XB24 | FOURCC_AB24 => false,
            other => bail!("unsupported scanout fourcc {other:#x}"),
        };

        let region = match region {
            Some(r) => clamp_rect(r, fw, fh).ok_or_else(|| anyhow!("region outside scanout"))?,
            None => Rect {
                x: 0,
                y: 0,
                width: fw,
                height: fh,
            },
        };

        let mut rgba = RgbaImage::new(region.width, region.height);
        let out = rgba.as_mut();
        let row_bytes = region.width as usize * 4;
        for row in 0..region.height as usize {
            let src_off = (region.y as usize + row) * stride + region.x as usize * 4;
            let src = &data[src_off..src_off + row_bytes];
            let dst = &mut out[row * row_bytes..(row + 1) * row_bytes];
            if swap_rb {
                for (d, s) in dst.chunks_exact_mut(4).zip(src.chunks_exact(4)) {
                    d[0] = s[2];
                    d[1] = s[1];
                    d[2] = s[0];
                    d[3] = 0xff;
                }
            } else {
                dst.copy_from_slice(src);
                for d in dst.chunks_exact_mut(4) {
                    d[3] = 0xff;
                }
            }
        }
        drop(frame);

        let mut cursor = None;
        if with_cursor {
            match self.tap.get_cursor() {
                Ok(c) => {
                    let info = CursorInfo {
                        visible: c.visible(),
                        x: c.x(),
                        y: c.y(),
                        hot_x: c.hot_x(),
                        hot_y: c.hot_y(),
                        width: c.width(),
                        height: c.height(),
                    };
                    if c.visible() {
                        if let Some(px) = c.pixels() {
                            composite_cursor(&mut rgba, &region, &info, px);
                        }
                    }
                    cursor = Some(info);
                }
                Err(e) => log::debug!("cursor unavailable this frame: {e}"),
            }
        }

        Ok(Shot {
            rgba,
            origin: (region.x, region.y),
            cursor,
            grab_ms: t0.elapsed().as_secs_f32() * 1000.0,
        })
    }

    /// Grab and encode. `max_side` bounds the longest side of the result.
    pub fn screenshot(
        &mut self,
        with_cursor: bool,
        region: Option<Rect>,
        max_side: Option<u32>,
        format: ImageFormat,
        quality: Option<u8>,
    ) -> Result<(ScreenshotInfo, Vec<u8>)> {
        let shot = self.grab(with_cursor, region)?;
        let t0 = Instant::now();
        let (sw, sh) = shot.rgba.dimensions();
        let (img, scale) = match max_side {
            Some(m) if m > 0 && (sw > m || sh > m) => {
                let s = (sw.max(sh) as f32) / m as f32;
                let nw = ((sw as f32 / s).round() as u32).max(1);
                let nh = ((sh as f32 / s).round() as u32).max(1);
                (
                    image::imageops::resize(&shot.rgba, nw, nh, FilterType::Triangle),
                    s,
                )
            }
            _ => (shot.rgba, 1.0),
        };
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
                let rgb = image::DynamicImage::ImageRgba8(img).into_rgb8();
                let enc = JpegEncoder::new_with_quality(&mut buf, quality.unwrap_or(85).clamp(1, 100));
                enc.write_image(rgb.as_raw(), w, h, image::ExtendedColorType::Rgb8)
                    .context("encode jpeg")?;
            }
        }
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
}

fn clamp_rect(r: Rect, fw: u32, fh: u32) -> Option<Rect> {
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

/// Premultiplied source-over of the cursor plane into the RGBA region.
fn composite_cursor(img: &mut RgbaImage, region: &Rect, c: &CursorInfo, px: &[u32]) {
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
            let cur = px[(row * c.width + col) as usize];
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
