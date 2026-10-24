//! Scanout capture through libdrmtap, one context per CRTC, stitched into a
//! single desktop image with the hardware cursor composited in.
//!
//! DRM knows each CRTC's mode and nothing about where the monitors sit next
//! to each other: compositors give every CRTC its own framebuffer, so every
//! display reports offset (0,0). The layout lives in the compositor, and
//! `cua layout` pushes it here ([`Capturer::set_layout`]). From it every
//! display gets a rectangle in *desktop space*, the one space screenshots and
//! input share: the primary display is 1:1 with its scanout, the others are
//! resampled by the ratio of their pixel densities. With one display, or
//! before a layout arrives, desktop space is the primary scanout, so the
//! single-display case is unchanged.
//!
//! Why desktop space is a uniform scaling of the compositor's logical layout:
//! compositors map an absolute pointer device's range linearly across the
//! whole logical desktop. Setting the uinput range to desktop space therefore
//! makes `ABS(x, y)` land on desktop pixel `(x, y)`, the pixel the screenshot
//! shows, on every monitor at once.
//!
//! The cursor is not in the scanout: it sits on its own KMS plane, visible on
//! exactly one CRTC at a time. Frame bytes come back from libdrmtap as
//! little-endian BGRX (DRM `XR24`), cursor pixels as premultiplied ARGB8888,
//! both B,G,R,(A) in memory.

use std::time::Instant;

use anyhow::{anyhow, bail, Context, Result};
use image::{codecs::jpeg::JpegEncoder, ImageEncoder, RgbaImage};
use kmscua_proto::{CursorInfo, DisplayInfo, ImageFormat, LayoutMonitor, Rect, ScreenshotInfo};
use libdrmtap::{Config, DrmTap};

const FOURCC_XR24: u32 = 0x3432_5258; // 'XR24' BGRX little-endian
const FOURCC_AR24: u32 = 0x3432_5241; // 'AR24' BGRA
const FOURCC_XB24: u32 = 0x3432_4258; // 'XB24' RGBX
const FOURCC_AB24: u32 = 0x3432_4241; // 'AB24' RGBA

pub const LAYOUT_COMPOSITOR: &str = "compositor";

/// One CRTC: its capture context and where it sits in desktop space.
struct Head {
    tap: DrmTap,
    info: DisplayInfo,
}

pub struct Capturer {
    device: Option<String>,
    helper: Option<String>,
    /// `--crtc`: capture only this CRTC. 0 = every active one.
    only_crtc: u32,
    heads: Vec<Head>,
    /// The head that is 1:1 with desktop space. Also answers `gpu_driver`
    /// and feeds the zero-copy recorder.
    primary: usize,
    cursor_ok: bool,
    gpu_driver: Option<String>,
    /// The layout last pushed; re-applied after a hotplug re-enumeration.
    monitors: Vec<LayoutMonitor>,
    layout: Option<String>,
}

/// A captured region as tightly packed RGBA plus where it came from.
pub struct Shot {
    pub rgba: RgbaImage,
    /// Desktop coordinates of the top-left pixel.
    pub origin: (i32, i32),
    /// In desktop coordinates.
    pub cursor: Option<CursorInfo>,
    pub grab_ms: f32,
}

/// The scanout as a dma-buf, for consumers that read it on the device
/// (the Jetson zero-copy recorder). The cursor plane comes along separately.
#[cfg_attr(not(feature = "jetson"), allow(dead_code))]
pub struct Scanout {
    pub frame: libdrmtap::Frame,
    /// Cursor position and premultiplied ARGB8888 pixels, when visible.
    pub cursor: Option<(CursorInfo, Vec<u32>)>,
    pub grabbed: Instant,
}

impl Capturer {
    pub fn open(device: Option<String>, crtc_id: u32, helper: Option<String>) -> Result<Self> {
        let mut s = Self {
            device,
            helper,
            only_crtc: crtc_id,
            heads: Vec::new(),
            primary: 0,
            cursor_ok: false,
            gpu_driver: None,
            monitors: Vec::new(),
            layout: None,
        };
        s.open_heads()?;
        Ok(s)
    }

    fn open_tap(&self, crtc_id: u32) -> Result<DrmTap> {
        let cfg = Config {
            device_path: self.device.clone(),
            crtc_id,
            helper_path: self.helper.clone(),
            debug: false,
        };
        DrmTap::open(Some(cfg)).map_err(|e| anyhow!("drmtap_open(crtc {crtc_id}): {e}"))
    }

    /// Enumerate the active CRTCs and open one context on each. The first
    /// context only enumerates: libdrmtap's auto-selection (crtc 0) takes
    /// the first active CRTC, which on a two-monitor desktop is as likely the
    /// side monitor as the main one, so every head is opened by explicit id.
    fn open_heads(&mut self) -> Result<()> {
        let mut probe = self.open_tap(self.only_crtc)?;
        self.gpu_driver = probe.gpu_driver();
        let listed = probe.list_displays().map_err(|e| anyhow!("list_displays: {e}"))?;
        drop(probe);

        let mut heads = Vec::new();
        for d in listed {
            if !d.active || d.crtc_id == 0 {
                continue;
            }
            if self.only_crtc != 0 && d.crtc_id != self.only_crtc {
                continue;
            }
            let mut tap = match self.open_tap(d.crtc_id) {
                Ok(t) => t,
                Err(e) => {
                    log::warn!("{}: {e:#}; skipping this display", d.name);
                    continue;
                }
            };
            // Pre-warm: the first grab pays for EGL context and detile setup
            // (44 ms measured on the Orin); nobody should pay that on a request.
            match tap.grab_mapped() {
                Ok(frame) => log::info!(
                    "{}: {}x{} stride={} fourcc={:#x} modifier={:#x}",
                    d.name,
                    frame.width(),
                    frame.height(),
                    frame.stride(),
                    frame.format(),
                    frame.modifier()
                ),
                Err(e) => log::warn!("{}: pre-warm grab failed (will retry per request): {e}", d.name),
            }
            heads.push(Head {
                tap,
                info: DisplayInfo {
                    name: d.name,
                    crtc_id: d.crtc_id,
                    connector_id: d.connector_id,
                    x: d.x,
                    y: d.y,
                    width: d.width,
                    height: d.height,
                    refresh_hz: d.refresh_hz,
                    active: d.active,
                    desktop: None,
                    scale: 1.0,
                },
            });
        }
        if heads.is_empty() {
            bail!("no active display");
        }
        // A new context has no topology hash yet, so its first
        // `displays_changed()` answers "changed" whatever the state. Prime it
        // now, or every later check would re-enumerate (and re-open fresh,
        // unprimed contexts: a loop).
        for h in &mut heads {
            let _ = h.tap.displays_changed();
        }
        self.heads = heads;
        self.cursor_ok = self.heads.iter_mut().any(|h| h.tap.get_cursor().is_ok());
        let monitors = std::mem::take(&mut self.monitors);
        if monitors.is_empty() {
            self.apply_default();
        } else if let Err(e) = self.set_layout(monitors) {
            log::warn!("layout no longer fits the displays ({e:#}); back to the single-display default until `cua layout` runs again");
            self.apply_default();
        }
        Ok(())
    }

    /// Single-display default: the first head is desktop space, nothing
    /// else is placed.
    fn apply_default(&mut self) {
        for (i, h) in self.heads.iter_mut().enumerate() {
            h.info.scale = 1.0;
            h.info.desktop = (i == 0).then_some(Rect {
                x: 0,
                y: 0,
                width: h.info.width,
                height: h.info.height,
            });
        }
        self.primary = 0;
        self.monitors.clear();
        self.layout = None;
        if self.heads.len() > 1 {
            log::info!(
                "{} active displays, no layout: capturing {} only; push the compositor layout with `cua layout`",
                self.heads.len(),
                self.heads[0].info.name
            );
        }
    }

    /// Place the heads from the compositor's layout. Returns the desktop rect.
    pub fn set_layout(&mut self, monitors: Vec<LayoutMonitor>) -> Result<Rect> {
        if monitors.is_empty() {
            self.apply_default();
            return self.desktop_rect();
        }
        let modes: Vec<HeadMode> = self
            .heads
            .iter()
            .map(|h| HeadMode { name: h.info.name.clone(), width: h.info.width, height: h.info.height })
            .collect();
        let plan = plan_layout(&modes, &monitors)?;
        for (h, place) in self.heads.iter_mut().zip(plan.placement) {
            h.info.desktop = place;
            h.info.scale = match place {
                Some(r) if r.width > 0 => h.info.width as f32 / r.width as f32,
                _ => 1.0,
            };
        }
        self.primary = plan.primary;
        self.monitors = monitors;
        self.layout = Some(LAYOUT_COMPOSITOR.to_string());
        let desktop = self.desktop_rect()?;
        log::info!(
            "layout: desktop {}x{} at ({},{}); {}",
            desktop.width,
            desktop.height,
            desktop.x,
            desktop.y,
            self.heads
                .iter()
                .map(|h| match h.info.desktop {
                    Some(r) => format!("{} at ({},{}) {}x{} x{:.3}", h.info.name, r.x, r.y, r.width, r.height, h.info.scale),
                    None => format!("{} unplaced", h.info.name),
                })
                .collect::<Vec<_>>()
                .join(", ")
        );
        Ok(desktop)
    }

    pub fn layout(&self) -> Option<String> {
        self.layout.clone()
    }

    /// Re-enumerate after a monitor was plugged, unplugged or re-moded.
    /// Returns true when the heads were rebuilt (the desktop rect may have
    /// changed and the pointer range must follow).
    pub fn check_hotplug(&mut self) -> bool {
        let changed = self.heads.get_mut(self.primary).map(|h| h.tap.displays_changed()).unwrap_or(false);
        if !changed {
            return false;
        }
        log::info!("display topology changed, re-enumerating");
        match self.open_heads() {
            Ok(()) => true,
            Err(e) => {
                log::warn!("re-enumeration failed: {e:#}; keeping the previous heads");
                false
            }
        }
    }

    pub fn gpu_driver(&self) -> Option<String> {
        self.gpu_driver.clone()
    }

    pub fn cursor_supported(&self) -> bool {
        self.cursor_ok
    }

    pub fn displays(&mut self) -> Result<Vec<DisplayInfo>> {
        Ok(self.heads.iter().map(|h| h.info.clone()).collect())
    }

    /// Union of the placed displays, in desktop pixels.
    pub fn desktop_rect(&mut self) -> Result<Rect> {
        union(self.heads.iter().filter_map(|h| h.info.desktop)).ok_or_else(|| anyhow!("no placed display"))
    }

    /// Where the cursor is, in desktop pixels: the one CRTC showing it, or
    /// the primary's (invisible) plane when none does.
    pub fn cursor(&mut self) -> Result<CursorInfo> {
        let mut hidden = None;
        let primary = self.primary;
        for (i, h) in self.heads.iter_mut().enumerate() {
            let Some(d) = h.info.desktop else { continue };
            let c = h.tap.get_cursor().map_err(|e| anyhow!("get_cursor({}): {e}", h.info.name))?;
            let info = to_desktop(&cursor_info(&c), &d, h.info.width, h.info.height);
            if c.visible() {
                return Ok(info);
            }
            if hidden.is_none() || i == primary {
                hidden = Some(info);
            }
        }
        hidden.ok_or_else(|| anyhow!("no placed display"))
    }

    /// Grab one frame of every display the region touches, composite each
    /// one's cursor, resample it into desktop space and paste.
    pub fn grab(&mut self, with_cursor: bool, region: Option<Rect>) -> Result<Shot> {
        let t0 = Instant::now();
        let desktop = self.desktop_rect()?;
        let region = match region {
            Some(r) => intersect(r, desktop).ok_or_else(|| anyhow!("region outside the desktop"))?,
            None => desktop,
        };
        let mut out = RgbaImage::new(region.width, region.height);
        let mut cursor: Option<CursorInfo> = None;
        let mut hidden: Option<CursorInfo> = None;
        for h in &mut self.heads {
            let Some(d) = h.info.desktop else { continue };
            let Some(inter) = intersect(region, d) else { continue };
            let (w, hgt) = (h.info.width, h.info.height);
            // The native sub-rect covering `inter`; floor/ceil so a fractional
            // scale never leaves a gap. The resampled piece is sized to `inter`,
            // which shifts it by under one desktop pixel on a scaled display.
            let fx = w as f64 / d.width as f64;
            let fy = hgt as f64 / d.height as f64;
            let nx0 = (((inter.x - d.x) as f64) * fx).floor().max(0.0) as u32;
            let ny0 = (((inter.y - d.y) as f64) * fy).floor().max(0.0) as u32;
            let nx1 = (((inter.x - d.x) as f64 + inter.width as f64) * fx).ceil().min(w as f64) as u32;
            let ny1 = (((inter.y - d.y) as f64 + inter.height as f64) * fy).ceil().min(hgt as f64) as u32;
            if nx1 <= nx0 || ny1 <= ny0 {
                continue;
            }
            let native = Rect { x: nx0 as i32, y: ny0 as i32, width: nx1 - nx0, height: ny1 - ny0 };
            let mut img = grab_native(&mut h.tap, &h.info.name, native)?;
            if with_cursor {
                match h.tap.get_cursor() {
                    Ok(c) => {
                        let info = cursor_info(&c);
                        if c.visible() {
                            if let Some(px) = c.pixels() {
                                composite_cursor(&mut img, &native, &info, px);
                            }
                            cursor = Some(to_desktop(&info, &d, w, hgt));
                        } else if hidden.is_none() {
                            hidden = Some(to_desktop(&info, &d, w, hgt));
                        }
                    }
                    Err(e) => log::debug!("{}: cursor unavailable this frame: {e}", h.info.name),
                }
            }
            let piece = if img.dimensions() == (inter.width, inter.height) {
                img
            } else {
                resample(&img, inter.width, inter.height)
            };
            image::imageops::replace(&mut out, &piece, (inter.x - region.x) as i64, (inter.y - region.y) as i64);
        }
        Ok(Shot {
            rgba: out,
            origin: (region.x, region.y),
            cursor: cursor.or(hidden),
            grab_ms: t0.elapsed().as_secs_f32() * 1000.0,
        })
    }

    /// Grab the primary scanout as a dma-buf without mapping or converting it.
    pub fn grab_scanout(&mut self, with_cursor: bool) -> Result<Scanout> {
        let grabbed = Instant::now();
        let head = self.heads.get_mut(self.primary).ok_or_else(|| anyhow!("no display"))?;
        let frame = head.tap.grab().map_err(|e| anyhow!("grab: {e}"))?;
        if frame.dma_buf_fd() < 0 {
            bail!("capture path returned no dma-buf");
        }
        let cursor = if with_cursor {
            head.tap.get_cursor().ok().and_then(|c| {
                let px = c.pixels().filter(|_| c.visible())?.to_vec();
                Some((cursor_info(&c), px))
            })
        } else {
            None
        };
        Ok(Scanout { frame, cursor, grabbed })
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
                let img = downscale_to(&shot.rgba, m);
                let s = sw as f32 / img.width() as f32;
                (img, s)
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

/// Grab one frame of a CRTC and convert `rect` of it to RGBA.
fn grab_native(tap: &mut DrmTap, name: &str, rect: Rect) -> Result<RgbaImage> {
    let frame = tap.grab_mapped().map_err(|e| anyhow!("grab_mapped({name}): {e}"))?;
    let (fw, fh, stride) = (frame.width(), frame.height(), frame.stride() as usize);
    let data = frame
        .data()
        .ok_or_else(|| anyhow!("grab_mapped returned no mapped data"))?;
    let swap_rb = match frame.format() {
        FOURCC_XR24 | FOURCC_AR24 => true,
        FOURCC_XB24 | FOURCC_AB24 => false,
        other => bail!("unsupported scanout fourcc {other:#x}"),
    };
    // The mode can change under us between enumeration and grab.
    let rect = clamp_rect(rect, fw, fh).ok_or_else(|| anyhow!("region outside the {name} scanout"))?;
    let mut rgba = RgbaImage::new(rect.width, rect.height);
    let out = rgba.as_mut();
    let row_bytes = rect.width as usize * 4;
    for row in 0..rect.height as usize {
        let src_off = (rect.y as usize + row) * stride + rect.x as usize * 4;
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
    Ok(rgba)
}

fn cursor_info(c: &libdrmtap::Cursor) -> CursorInfo {
    CursorInfo {
        visible: c.visible(),
        x: c.x(),
        y: c.y(),
        hot_x: c.hot_x(),
        hot_y: c.hot_y(),
        width: c.width(),
        height: c.height(),
    }
}

/// A cursor read on one CRTC, moved into desktop space.
fn to_desktop(c: &CursorInfo, d: &Rect, scan_w: u32, scan_h: u32) -> CursorInfo {
    let fx = d.width as f64 / scan_w.max(1) as f64;
    let fy = d.height as f64 / scan_h.max(1) as f64;
    let sx = |v: i32| (v as f64 * fx).round() as i32;
    let sy = |v: i32| (v as f64 * fy).round() as i32;
    CursorInfo {
        visible: c.visible,
        x: d.x + sx(c.x),
        y: d.y + sy(c.y),
        hot_x: sx(c.hot_x),
        hot_y: sy(c.hot_y),
        width: ((c.width as f64 * fx).round() as u32).max(1),
        height: ((c.height as f64 * fy).round() as u32).max(1),
    }
}

// ---------- layout planning (pure, tested below) ----------

pub struct HeadMode {
    pub name: String,
    pub width: u32,
    pub height: u32,
}

pub struct Plan {
    pub primary: usize,
    /// Per head, in the heads' order.
    pub placement: Vec<Option<Rect>>,
}

/// Match the compositor's monitors to the DRM heads and scale the layout so
/// the primary head is 1:1. Heads the layout does not mention stay unplaced;
/// monitors with no head are ignored with a warning.
pub fn plan_layout(heads: &[HeadMode], monitors: &[LayoutMonitor]) -> Result<Plan> {
    if heads.is_empty() {
        bail!("no active display");
    }
    // head index -> monitor index
    let mut pair: Vec<Option<usize>> = vec![None; heads.len()];
    let mut used = vec![false; monitors.len()];
    for (hi, h) in heads.iter().enumerate() {
        let key = connector_key(&h.name);
        if let Some(mi) = monitors.iter().position(|m| connector_key(&m.connector) == key) {
            pair[hi] = Some(mi);
            used[mi] = true;
        }
    }
    // One head and one monitor left over: they are each other's.
    let free_heads: Vec<usize> = (0..heads.len()).filter(|&i| pair[i].is_none()).collect();
    let free_mons: Vec<usize> = (0..monitors.len()).filter(|&i| !used[i]).collect();
    if free_heads.len() == 1 && free_mons.len() == 1 {
        log::info!(
            "pairing display {} with layout monitor {} by elimination",
            heads[free_heads[0]].name,
            monitors[free_mons[0]].connector
        );
        pair[free_heads[0]] = Some(free_mons[0]);
        used[free_mons[0]] = true;
    }
    for (mi, m) in monitors.iter().enumerate() {
        if !used[mi] {
            log::warn!("layout monitor {} matches no active display; ignored", m.connector);
        }
    }
    if pair.iter().all(|p| p.is_none()) {
        bail!(
            "no layout monitor matches a display (displays: {}; layout: {})",
            heads.iter().map(|h| h.name.as_str()).collect::<Vec<_>>().join(", "),
            monitors.iter().map(|m| m.connector.as_str()).collect::<Vec<_>>().join(", ")
        );
    }
    for (hi, p) in pair.iter().enumerate() {
        let Some(mi) = p else { continue };
        let (h, m) = (&heads[hi], &monitors[*mi]);
        if m.width == 0 || m.height == 0 {
            bail!("layout monitor {} has an empty rectangle", m.connector);
        }
        let head_aspect = h.width as f64 / h.height as f64;
        let mon_aspect = m.width as f64 / m.height as f64;
        if (head_aspect / mon_aspect - 1.0).abs() > 0.02 {
            bail!(
                "{} is rotated or transformed in the layout ({}x{} logical for a {}x{} mode); not supported yet",
                h.name, m.width, m.height, h.width, h.height
            );
        }
    }
    let primary = (0..heads.len())
        .find(|&hi| pair[hi].map(|mi| monitors[mi].primary).unwrap_or(false))
        .or_else(|| (0..heads.len()).find(|&hi| pair[hi].is_some()))
        .expect("a matched head exists");
    let pm = &monitors[pair[primary].unwrap()];
    let k = heads[primary].width as f64 / pm.width as f64;
    let placement = pair
        .iter()
        .enumerate()
        .map(|(hi, p)| {
            p.map(|mi| {
                let m = &monitors[mi];
                let r = |v: i64| (v as f64 * k).round() as i64;
                if hi == primary {
                    Rect { x: r(m.x as i64) as i32, y: r(m.y as i64) as i32, width: heads[hi].width, height: heads[hi].height }
                } else {
                    Rect {
                        x: r(m.x as i64) as i32,
                        y: r(m.y as i64) as i32,
                        width: r(m.width as i64).max(1) as u32,
                        height: r(m.height as i64).max(1) as u32,
                    }
                }
            })
        })
        .collect();
    Ok(Plan { primary, placement })
}

/// The kernel says "HDMI-A-1" where Mutter and xrandr say "HDMI-1"; every
/// other connector type is spelled the same on both sides.
pub fn connector_key(name: &str) -> String {
    let n = name.trim().to_ascii_uppercase();
    match n.strip_prefix("HDMI-A-") {
        Some(rest) => format!("HDMI-{rest}"),
        None => n,
    }
}

fn union(rects: impl Iterator<Item = Rect>) -> Option<Rect> {
    let mut acc: Option<(i64, i64, i64, i64)> = None;
    for r in rects {
        let (x0, y0, x1, y1) = (r.x as i64, r.y as i64, r.x as i64 + r.width as i64, r.y as i64 + r.height as i64);
        acc = Some(match acc {
            None => (x0, y0, x1, y1),
            Some((a, b, c, d)) => (a.min(x0), b.min(y0), c.max(x1), d.max(y1)),
        });
    }
    acc.filter(|(x0, y0, x1, y1)| x1 > x0 && y1 > y0)
        .map(|(x0, y0, x1, y1)| Rect { x: x0 as i32, y: y0 as i32, width: (x1 - x0) as u32, height: (y1 - y0) as u32 })
}

fn intersect(a: Rect, b: Rect) -> Option<Rect> {
    let x0 = a.x.max(b.x) as i64;
    let y0 = a.y.max(b.y) as i64;
    let x1 = (a.x as i64 + a.width as i64).min(b.x as i64 + b.width as i64);
    let y1 = (a.y as i64 + a.height as i64).min(b.y as i64 + b.height as i64);
    (x1 > x0 && y1 > y0).then(|| Rect { x: x0 as i32, y: y0 as i32, width: (x1 - x0) as u32, height: (y1 - y0) as u32 })
}

// ---------- resampling ----------

/// Resize to exactly `dw` x `dh` (bilinear; the per-display factors are
/// between about 0.5 and 2, where bilinear neither aliases nor blurs much).
pub fn resample(src: &RgbaImage, dw: u32, dh: u32) -> RgbaImage {
    let (sw, sh) = src.dimensions();
    let (dw, dh) = (dw.max(1), dh.max(1));
    if (sw, sh) == (dw, dh) {
        return src.clone();
    }
    use fast_image_resize as fr;
    let src_img = fr::images::ImageRef::new(sw, sh, src.as_raw(), fr::PixelType::U8x4).expect("image ref");
    let mut dst = fr::images::Image::new(dw, dh, fr::PixelType::U8x4);
    let opts = fr::ResizeOptions::new().resize_alg(fr::ResizeAlg::Convolution(fr::FilterType::Bilinear));
    fr::Resizer::new().resize(&src_img, &mut dst, &opts).expect("resize");
    RgbaImage::from_raw(dw, dh, dst.into_vec()).expect("sized buffer")
}

/// Downscale so the longest side equals `max_side` (aspect kept).
///
/// Two stages, both measured on the Orin for 4K input: an integer box filter
/// with rows split across cores does the bulk of the reduction (about 5 ms
/// for 2x), then `fast_image_resize` (NEON, rayon) does the residual
/// non-integer step on the already small image (a few ms). The image
/// crate's own resampler took 134 ms for the same 4K -> 1080p.
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
    resample(&coarse, dw, dh)
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
    let rows_per = (dh as usize + threads - 1) / threads;
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

/// Premultiplied source-over of the cursor plane into the RGBA region
/// (both in the same CRTC's scanout pixels).
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

#[cfg(test)]
mod tests {
    use super::*;

    fn head(name: &str, w: u32, h: u32) -> HeadMode {
        HeadMode { name: name.into(), width: w, height: h }
    }

    fn mon(c: &str, x: i32, y: i32, w: u32, h: u32, primary: bool) -> LayoutMonitor {
        LayoutMonitor { connector: c.into(), x, y, width: w, height: h, primary }
    }

    /// The machine this was written on: a 4K 32" at 1.25x as primary and a
    /// 4K 18" at 1.667x to its right, as Mutter reports them in logical px.
    #[test]
    fn two_scales_primary_is_one_to_one() {
        let heads = [head("HDMI-A-1", 3840, 2160), head("DP-2", 3840, 2160)];
        let mons = [mon("DP-2", 0, 0, 3072, 1728, true), mon("HDMI-1", 3072, 0, 2304, 1296, false)];
        let p = plan_layout(&heads, &mons).unwrap();
        assert_eq!(p.primary, 1);
        assert_eq!(p.placement[1], Some(Rect { x: 0, y: 0, width: 3840, height: 2160 }));
        // k = 3840/3072 = 1.25: the side monitor is 2304*1.25 = 2880 wide at x 3840.
        assert_eq!(p.placement[0], Some(Rect { x: 3840, y: 0, width: 2880, height: 1620 }));
    }

    /// xrandr under Xwayland reports the same layout at a uniform 2x; the
    /// result must not change.
    #[test]
    fn uniform_scale_of_the_layout_is_irrelevant() {
        let heads = [head("HDMI-A-1", 3840, 2160), head("DP-2", 3840, 2160)];
        let mons = [mon("DP-2", 0, 0, 6144, 3456, true), mon("HDMI-1", 6144, 0, 4608, 2592, false)];
        let p = plan_layout(&heads, &mons).unwrap();
        assert_eq!(p.placement[0], Some(Rect { x: 3840, y: 0, width: 2880, height: 1620 }));
    }

    #[test]
    fn single_display_is_identity() {
        let heads = [head("eDP-1", 2560, 1600)];
        let p = plan_layout(&heads, &[mon("eDP-1", 0, 0, 1280, 800, true)]).unwrap();
        assert_eq!(p.placement[0], Some(Rect { x: 0, y: 0, width: 2560, height: 1600 }));
    }

    #[test]
    fn no_primary_flag_takes_the_first_matched() {
        let heads = [head("DP-1", 1920, 1080), head("DP-2", 1920, 1080)];
        let mons = [mon("DP-2", 1920, 0, 1920, 1080, false), mon("DP-1", 0, 0, 1920, 1080, false)];
        let p = plan_layout(&heads, &mons).unwrap();
        assert_eq!(p.primary, 0);
        assert_eq!(p.placement[1], Some(Rect { x: 1920, y: 0, width: 1920, height: 1080 }));
    }

    #[test]
    fn leftover_pair_matches_by_elimination() {
        // The proprietary X driver spells connectors its own way.
        let heads = [head("DP-2", 3840, 2160), head("HDMI-A-1", 3840, 2160)];
        let mons = [mon("DP-2", 0, 0, 3840, 2160, true), mon("HDMI-0", 3840, 0, 3840, 2160, false)];
        let p = plan_layout(&heads, &mons).unwrap();
        assert_eq!(p.placement[1], Some(Rect { x: 3840, y: 0, width: 3840, height: 2160 }));
    }

    #[test]
    fn unmentioned_head_stays_unplaced_and_unknown_monitor_is_ignored() {
        let heads = [head("DP-1", 1920, 1080), head("DP-2", 1920, 1080), head("DP-3", 1920, 1080)];
        let mons = [mon("DP-1", 0, 0, 1920, 1080, true), mon("VGA-1", 1920, 0, 1024, 768, false)];
        // Two free heads, one free monitor: no elimination.
        let p = plan_layout(&heads, &mons).unwrap();
        assert_eq!(p.placement[1], None);
        assert_eq!(p.placement[2], None);
    }

    #[test]
    fn rotated_display_is_refused() {
        let heads = [head("DP-1", 3840, 2160)];
        assert!(plan_layout(&heads, &[mon("DP-1", 0, 0, 1080, 1920, true)]).is_err());
    }

    #[test]
    fn nothing_matches_is_an_error() {
        // Two of each and no name in common: elimination cannot pair them.
        let heads = [head("DP-1", 1920, 1080), head("DP-2", 1920, 1080)];
        let mons = [mon("HDMI-1", 0, 0, 1920, 1080, true), mon("HDMI-2", 1920, 0, 1920, 1080, false)];
        assert!(plan_layout(&heads, &mons).is_err());
    }

    #[test]
    fn single_head_takes_the_single_monitor_whatever_its_name() {
        let heads = [head("DP-1", 1920, 1080)];
        let p = plan_layout(&heads, &[mon("HDMI-1", 0, 0, 960, 540, true)]).unwrap();
        assert_eq!(p.placement[0], Some(Rect { x: 0, y: 0, width: 1920, height: 1080 }));
    }

    #[test]
    fn connector_keys() {
        assert_eq!(connector_key("HDMI-A-1"), "HDMI-1");
        assert_eq!(connector_key("hdmi-1"), "HDMI-1");
        assert_eq!(connector_key("DP-2"), "DP-2");
        assert_eq!(connector_key("eDP-1"), "EDP-1");
        assert_eq!(connector_key("HDMI-B-2"), "HDMI-B-2");
    }

    #[test]
    fn cursor_moves_into_desktop_space() {
        let d = Rect { x: 3840, y: 0, width: 2880, height: 1620 };
        let c = CursorInfo { visible: true, x: 3280, y: 1440, hot_x: 4, hot_y: 8, width: 32, height: 32 };
        let t = to_desktop(&c, &d, 3840, 2160);
        assert_eq!((t.x, t.y), (3840 + 2460, 1080));
        assert_eq!((t.hot_x, t.hot_y, t.width), (3, 6, 24));
    }

    #[test]
    fn union_and_intersect() {
        let a = Rect { x: 0, y: 0, width: 3840, height: 2160 };
        let b = Rect { x: 3840, y: 0, width: 2880, height: 1620 };
        assert_eq!(union([a, b].into_iter()), Some(Rect { x: 0, y: 0, width: 6720, height: 2160 }));
        assert_eq!(intersect(a, b), None);
        let r = Rect { x: 3000, y: 100, width: 2000, height: 100 };
        assert_eq!(intersect(r, a), Some(Rect { x: 3000, y: 100, width: 840, height: 100 }));
        assert_eq!(intersect(r, b), Some(Rect { x: 3840, y: 100, width: 1160, height: 100 }));
    }

    #[test]
    fn resample_exact_size() {
        let src = RgbaImage::from_pixel(100, 50, image::Rgba([10, 20, 30, 255]));
        let out = resample(&src, 75, 38);
        assert_eq!(out.dimensions(), (75, 38));
        assert_eq!(out.get_pixel(10, 10).0, [10, 20, 30, 255]);
    }
}
