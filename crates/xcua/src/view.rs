//! The coordinate view between CLI invocations: what the last `xcua
//! screenshot` looked like, so `xcua click 640 400` means pixel (640, 400)
//! of that image, exactly as the MCP tools read coordinates.
//!
//! `screenshot` writes `<state>/view.json` (scale, origin, image and source
//! size, max_side, display). Every verb that takes coordinates maps them
//! through it with the MCP layer's formula: `desktop = round(px * scale) +
//! origin`. No view, or a view of another display or of a screen that has
//! since changed size, means 1:1 (screenshot pixels are screen pixels), with
//! a warning when a stale view was set aside.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use kmscua_proto::{Rect, ScreenshotInfo};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct View {
    /// The display the screenshot came from.
    pub display: String,
    /// Screen pixels per image pixel.
    pub scale: f32,
    pub origin_x: i32,
    pub origin_y: i32,
    /// Size of the image the coordinates refer to.
    pub width: u32,
    pub height: u32,
    /// Size of the screen region it was taken from.
    pub source_width: u32,
    pub source_height: u32,
    /// The `--max-side` the screenshot was taken with, if any.
    pub max_side: Option<u32>,
}

impl View {
    /// 1:1 over a `w` x `h` screen: what coordinates mean with no screenshot.
    pub fn identity(display: &str, (w, h): (u32, u32)) -> Self {
        Self {
            display: display.to_string(),
            scale: 1.0,
            origin_x: 0,
            origin_y: 0,
            width: w,
            height: h,
            source_width: w,
            source_height: h,
            max_side: None,
        }
    }

    pub fn from_info(display: &str, info: &ScreenshotInfo, max_side: Option<u32>) -> Self {
        Self {
            display: display.to_string(),
            scale: info.scale,
            origin_x: info.origin_x,
            origin_y: info.origin_y,
            width: info.width,
            height: info.height,
            source_width: info.source_width,
            source_height: info.source_height,
            max_side,
        }
    }

    /// Screenshot pixel to screen pixel (the MCP layer's `to_scanout`).
    pub fn to_desktop(&self, x: i32, y: i32) -> (i32, i32) {
        (
            (x as f32 * self.scale).round() as i32 + self.origin_x,
            (y as f32 * self.scale).round() as i32 + self.origin_y,
        )
    }

    /// A rect of screenshot pixels to screen pixels.
    pub fn rect_to_desktop(&self, x: i32, y: i32, w: i32, h: i32) -> Rect {
        let (sx, sy) = self.to_desktop(x, y);
        Rect {
            x: sx,
            y: sy,
            width: (w as f32 * self.scale).round().max(1.0) as u32,
            height: (h as f32 * self.scale).round().max(1.0) as u32,
        }
    }

    /// Screen pixel back to screenshot pixel (for `cursor`).
    pub fn from_desktop(&self, x: i32, y: i32) -> (i32, i32) {
        (
            ((x - self.origin_x) as f32 / self.scale).round() as i32,
            ((y - self.origin_y) as f32 / self.scale).round() as i32,
        )
    }

    pub fn path(state_dir: &Path) -> PathBuf {
        state_dir.join("view.json")
    }

    pub fn save(&self, state_dir: &Path) -> Result<()> {
        std::fs::create_dir_all(state_dir).with_context(|| format!("create {}", state_dir.display()))?;
        let p = Self::path(state_dir);
        let tmp = p.with_extension(format!("json.{}.tmp", std::process::id()));
        std::fs::write(&tmp, serde_json::to_vec_pretty(self)?).with_context(|| format!("write {}", tmp.display()))?;
        std::fs::rename(&tmp, &p).with_context(|| format!("rename to {}", p.display()))
    }

    /// The view coordinates refer to on `display`, whose screen is `screen`
    /// now, plus a warning when a saved view was set aside.
    pub fn load(state_dir: &Path, display: &str, screen: (u32, u32)) -> (Self, Option<String>) {
        let p = Self::path(state_dir);
        let Ok(text) = std::fs::read_to_string(&p) else { return (Self::identity(display, screen), None) };
        let v: View = match serde_json::from_str(&text) {
            Ok(v) => v,
            Err(e) => {
                return (Self::identity(display, screen), Some(format!("ignoring {} ({e}); coordinates are screen pixels", p.display())))
            }
        };
        if v.display != display {
            let w = format!("the last screenshot was of {}, not {display}; coordinates are screen pixels until you take one", v.display);
            return (Self::identity(display, screen), Some(w));
        }
        if v.origin_x == 0 && v.origin_y == 0 && (v.source_width, v.source_height) != screen || v.scale <= 0.0 {
            let w = format!(
                "the screen is {}x{} but the last screenshot was of {}x{}; coordinates are screen pixels until you take a new one",
                screen.0, screen.1, v.source_width, v.source_height
            );
            return (Self::identity(display, screen), Some(w));
        }
        (v, None)
    }
}

/// "x,y,w,h" to four integers.
pub fn parse_xywh(s: &str) -> Result<[i32; 4]> {
    let v: Vec<i32> = s
        .split(',')
        .map(|p| p.trim().parse::<i32>())
        .collect::<std::result::Result<_, _>>()
        .with_context(|| format!("region must be x,y,w,h, not {s:?}"))?;
    let [x, y, w, h] = v[..] else { anyhow::bail!("region must be x,y,w,h, not {s:?}") };
    if w <= 0 || h <= 0 {
        anyhow::bail!("region {s:?} has no area");
    }
    Ok([x, y, w, h])
}

#[cfg(test)]
mod tests {
    use super::*;

    fn info(scale: f32, w: u32, h: u32, sw: u32, sh: u32) -> ScreenshotInfo {
        ScreenshotInfo {
            width: w,
            height: h,
            source_width: sw,
            source_height: sh,
            scale,
            origin_x: 0,
            origin_y: 0,
            mime: "image/png".into(),
            cursor: None,
            grab_ms: 0.0,
            encode_ms: 0.0,
        }
    }

    #[test]
    fn maps_like_the_mcp_layer() {
        // 1920x1080 shot at --max-side 1280: 1.5 screen px per image px.
        let v = View::from_info(":3", &info(1.5, 1280, 720, 1920, 1080), Some(1280));
        assert_eq!(v.to_desktop(640, 400), (960, 600));
        assert_eq!(v.to_desktop(0, 0), (0, 0));
        assert_eq!(v.to_desktop(1279, 719), (1919, 1079));
        assert_eq!(v.from_desktop(960, 600), (640, 400));
        let r = v.rect_to_desktop(100, 100, 10, 20);
        assert_eq!((r.x, r.y, r.width, r.height), (150, 150, 15, 30));
        let id = View::identity(":3", (800, 600));
        assert_eq!(id.to_desktop(13, 17), (13, 17));
    }

    #[test]
    fn load_sets_stale_views_aside() {
        let dir = std::env::temp_dir().join(format!("xcua-view-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let (v, w) = View::load(&dir, ":3", (1920, 1080));
        assert_eq!((v.scale, w), (1.0, None));
        View::from_info(":3", &info(1.5, 1280, 720, 1920, 1080), Some(1280)).save(&dir).unwrap();
        let (v, w) = View::load(&dir, ":3", (1920, 1080));
        assert_eq!((v.scale, v.max_side, w), (1.5, Some(1280), None));
        // Another display, or a resized screen: 1:1 with a warning.
        let (v, w) = View::load(&dir, ":4", (1920, 1080));
        assert!(v.scale == 1.0 && w.unwrap().contains(":3"));
        let (v, w) = View::load(&dir, ":3", (1280, 800));
        assert!(v.scale == 1.0 && w.unwrap().contains("1280x800"));
        std::fs::write(View::path(&dir), "{").unwrap();
        let (v, w) = View::load(&dir, ":3", (1920, 1080));
        assert!(v.scale == 1.0 && w.is_some());
        std::fs::remove_dir_all(&dir).unwrap();
        assert_eq!(parse_xywh("1, 2,3,4").unwrap(), [1, 2, 3, 4]);
        assert!(parse_xywh("1,2,3").is_err() && parse_xywh("1,2,0,4").is_err());
    }
}
