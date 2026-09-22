//! MCP stdio server, shaped like the tools Claude models were trained on.
//!
//! The flat tool family of Anthropic's `computer_toolset_20260801` and Claude
//! Code's own computer-use MCP: `screenshot, zoom, left_click, right_click,
//! middle_click, double_click, triple_click, left_click_drag, mouse_move,
//! left_mouse_down, left_mouse_up, cursor_position, scroll, type, key,
//! hold_key, wait, computer_batch`, with the same parameter names
//! (`coordinate: [x, y]`, `start_coordinate`, `text`, `scroll_direction`,
//! `scroll_amount`, `duration`, `repeat`, `region`) and the same return
//! conventions: actions answer `OK`, `screenshot` answers with the image
//! only, `cursor_position` answers `X=…, Y=…`.
//!
//! On top of that, extras no vendor ships and agents benefit from:
//! `wait_for_stable`, `wait_for_change`, `record_*`, `doctor`. Every action
//! also accepts an optional `settle` flag that appends the settled screenshot.
//!
//! Coordinates a model passes are pixels of the last full `screenshot`; this
//! layer converts them to scanout pixels. `zoom` never changes that space.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use base64::Engine;
use kmscua_proto::{
    Button, Client, CursorInfo, ImageFormat, RecordInfo, Rect, Request, ScreenshotInfo,
};
use rmcp::{
    handler::server::{router::tool::ToolRouter, wrapper::Parameters},
    model::{CallToolResult, Content, Implementation, ServerCapabilities, ServerInfo},
    tool, tool_handler, tool_router, ErrorData, ServerHandler, ServiceExt,
};
use schemars::JsonSchema;
use serde::Deserialize;

use crate::video;

/// Default longest side of a screenshot. 1080p is Anthropic's documented
/// balance of accuracy and cost; the models accept up to 2576 px.
const DEFAULT_MAX_SIDE: u32 = 1920;
const NOT_EXECUTED: &str = "Not executed: an earlier computer action in this turn failed.";

/// Scale and origin of the last full screenshot: image px -> scanout px.
#[derive(Clone, Copy)]
struct View {
    scale: f32,
    ox: i32,
    oy: i32,
}

struct RecState {
    started: Instant,
    path: PathBuf,
    timeline: Vec<(f32, String)>,
}

#[derive(Clone)]
pub struct CuaServer {
    client: Arc<Client>,
    view: Arc<Mutex<View>>,
    rec: Arc<Mutex<Option<RecState>>>,
    last_rec: Arc<Mutex<Option<PathBuf>>>,
    max_side: u32,
    tool_router: ToolRouter<Self>,
}

fn err(e: anyhow::Error) -> ErrorData {
    ErrorData::internal_error(format!("{e:#}"), None)
}

fn bad(msg: impl Into<String>) -> ErrorData {
    ErrorData::invalid_params(msg.into(), None)
}

// ---------- params: Claude family ----------

#[derive(Deserialize, JsonSchema, Default)]
pub struct ScreenshotParams {
    /// After capturing, wait until the screen stops changing first.
    pub settle: Option<bool>,
}

#[derive(Deserialize, JsonSchema)]
pub struct ZoomParams {
    /// Region of the last screenshot to magnify: [x0, y0, x1, y1]. Coordinates
    /// you pass to other tools still refer to the full-screen screenshot,
    /// never the zoomed image.
    pub region: [i32; 4],
}

#[derive(Deserialize, JsonSchema)]
pub struct ClickParams {
    /// [x, y] in the last screenshot's pixels.
    pub coordinate: [i32; 2],
    /// Modifier keys to hold during the click, e.g. "shift" or "ctrl+shift".
    pub text: Option<String>,
    /// Wait for the screen to settle afterwards and return a screenshot.
    pub settle: Option<bool>,
}

#[derive(Deserialize, JsonSchema)]
pub struct MoveParams {
    /// [x, y] in the last screenshot's pixels.
    pub coordinate: [i32; 2],
}

#[derive(Deserialize, JsonSchema)]
pub struct DragParams {
    /// [x, y] where the drag ends.
    pub coordinate: [i32; 2],
    /// [x, y] where the drag starts. Omit to drag from the current cursor.
    pub start_coordinate: Option<[i32; 2]>,
    /// Modifier keys to hold during the drag.
    pub text: Option<String>,
    pub settle: Option<bool>,
}

#[derive(Deserialize, JsonSchema)]
pub struct ScrollParams {
    /// [x, y] to scroll at. Omit to scroll at the current cursor.
    pub coordinate: Option<[i32; 2]>,
    /// "up", "down", "left" or "right".
    pub scroll_direction: String,
    /// Number of wheel ticks, 0-100.
    pub scroll_amount: u32,
    /// Modifier keys to hold while scrolling.
    pub text: Option<String>,
    pub settle: Option<bool>,
}

#[derive(Deserialize, JsonSchema)]
pub struct TypeParams {
    /// Literal text to type.
    pub text: String,
    pub settle: Option<bool>,
}

#[derive(Deserialize, JsonSchema)]
pub struct KeyParams {
    /// xdotool-style key or chord: "Return", "ctrl+s", "alt+Tab", "Page_Down".
    pub text: String,
    /// Press the chord this many times, 1-100.
    pub repeat: Option<u32>,
    pub settle: Option<bool>,
}

#[derive(Deserialize, JsonSchema)]
pub struct HoldKeyParams {
    /// xdotool-style key or chord to hold.
    pub text: String,
    /// Seconds to hold, 0-100.
    pub duration: f32,
}

#[derive(Deserialize, JsonSchema)]
pub struct WaitParams {
    /// Seconds to wait, 0-100.
    pub duration: f32,
}

/// One item of `computer_batch`: the legacy single-tool action schema.
#[derive(Deserialize, JsonSchema, Clone)]
pub struct BatchAction {
    /// key | type | mouse_move | left_click | left_click_drag | right_click |
    /// middle_click | double_click | triple_click | scroll | hold_key |
    /// screenshot | cursor_position | left_mouse_down | left_mouse_up | wait | zoom
    pub action: String,
    pub coordinate: Option<[i32; 2]>,
    pub start_coordinate: Option<[i32; 2]>,
    pub text: Option<String>,
    pub scroll_direction: Option<String>,
    pub scroll_amount: Option<u32>,
    pub duration: Option<f32>,
    pub repeat: Option<u32>,
    pub region: Option<[i32; 4]>,
}

#[derive(Deserialize, JsonSchema)]
pub struct BatchParams {
    /// Executed in order; stops at the first failure. End with a screenshot
    /// action to see the result.
    pub actions: Vec<BatchAction>,
}

// ---------- params: extras ----------

#[derive(Deserialize, JsonSchema)]
pub struct WaitStableParams {
    /// Region to watch in the last screenshot's pixels: [x, y, width, height].
    /// Default: whole screen. Use it to ignore a clock or a blinking caret.
    pub region: Option<[i32; 4]>,
    /// How long the region must stay unchanged, default 600 ms.
    pub quiet_ms: Option<u32>,
    /// Give up after this long, default 8000 ms.
    pub timeout_ms: Option<u32>,
    /// Fraction of pixels allowed to differ between samples, default 0.002.
    pub threshold: Option<f32>,
}

#[derive(Deserialize, JsonSchema)]
pub struct WaitChangeParams {
    /// Region to watch in the last screenshot's pixels: [x, y, width, height].
    pub region: Option<[i32; 4]>,
    /// Give up after this long, default 8000 ms.
    pub timeout_ms: Option<u32>,
    /// Fraction of pixels that must differ from the starting frame, default 0.01.
    pub threshold: Option<f32>,
}

#[derive(Deserialize, JsonSchema)]
pub struct RecordStartParams {
    /// Base name for the file (no extension). Default: timestamp.
    pub name: Option<String>,
    /// Frames per second, default 15.
    pub fps: Option<u32>,
    /// Longest side of the video, default 1920.
    pub max_side: Option<u32>,
    /// Auto-stop after this many seconds, default 600.
    pub max_seconds: Option<u32>,
}

#[derive(Deserialize, JsonSchema)]
pub struct RecordStopParams {
    /// Return a contact sheet of evenly spaced frames (default true).
    pub sheet: Option<bool>,
    /// Number of tiles on the sheet, default 6.
    pub tiles: Option<u32>,
}

#[derive(Deserialize, JsonSchema)]
pub struct RecordMarkParams {
    /// Free text stamped into the timeline at the current time.
    pub note: String,
}

#[derive(Deserialize, JsonSchema)]
pub struct RecordFramesParams {
    /// Recording file; default: the last one stopped in this session.
    pub path: Option<String>,
    /// Exact times in seconds to extract.
    pub at: Option<Vec<f32>>,
    /// Extract one frame every N seconds.
    pub every: Option<f32>,
    /// Extract frames where the picture changed at least this much (0..1).
    /// UI changes are small: 0.05-0.1 catches a tab switch. Overrides `every`.
    pub scene: Option<f32>,
    /// Between these two timeline entries (indices from record_stop).
    pub between: Option<[u32; 2]>,
    /// Max frames returned, default 8.
    pub max: Option<u32>,
    /// Longest side of each frame, default 800.
    pub max_side: Option<u32>,
}

// ---------- helpers ----------

fn modifiers(text: &Option<String>) -> Vec<String> {
    match text.as_deref().map(str::trim) {
        Some(t) if !t.is_empty() => vec![t.to_string()],
        _ => vec![],
    }
}

fn ok() -> Result<CallToolResult, ErrorData> {
    Ok(CallToolResult::success(vec![Content::text("OK")]))
}

impl CuaServer {
    fn to_scanout(&self, x: i32, y: i32) -> (i32, i32) {
        let v = *self.view.lock().unwrap();
        (
            (x as f32 * v.scale).round() as i32 + v.ox,
            (y as f32 * v.scale).round() as i32 + v.oy,
        )
    }

    fn rect_to_scanout(&self, x: i32, y: i32, w: i32, h: i32) -> Rect {
        let (sx, sy) = self.to_scanout(x, y);
        let s = self.view.lock().unwrap().scale;
        Rect {
            x: sx,
            y: sy,
            width: (w as f32 * s).round().max(1.0) as u32,
            height: (h as f32 * s).round().max(1.0) as u32,
        }
    }

    fn region_xywh(&self, r: Option<[i32; 4]>) -> Option<Rect> {
        r.map(|[x, y, w, h]| self.rect_to_scanout(x, y, w, h))
    }

    async fn blocking<T: Send + 'static>(
        &self,
        f: impl FnOnce(&Client) -> anyhow::Result<T> + Send + 'static,
    ) -> Result<T, ErrorData> {
        let client = self.client.clone();
        tokio::task::spawn_blocking(move || f(&client))
            .await
            .map_err(|e| ErrorData::internal_error(e.to_string(), None))?
            .map_err(err)
    }

    fn mark(&self, what: impl Into<String>) {
        if let Some(r) = self.rec.lock().unwrap().as_mut() {
            r.timeline.push((r.started.elapsed().as_secs_f32(), what.into()));
        }
    }

    async fn cursor_scanout(&self) -> Result<(i32, i32), ErrorData> {
        let reply = self.blocking(|c| c.call(&Request::Cursor)).await?;
        let c: CursorInfo = reply.data().map_err(err)?;
        Ok((c.x + c.hot_x, c.y + c.hot_y))
    }

    /// Full screenshot. Updates the coordinate view. Image only.
    async fn shot_full(&self) -> Result<Content, ErrorData> {
        let max_side = self.max_side;
        let reply = self
            .blocking(move |c| {
                c.call(&Request::Screenshot {
                    max_side: Some(max_side),
                    format: ImageFormat::Png,
                    quality: None,
                    cursor: Some(true),
                    region: None,
                })
            })
            .await?;
        let info: ScreenshotInfo = reply.data().map_err(err)?;
        *self.view.lock().unwrap() = View {
            scale: info.scale,
            ox: info.origin_x,
            oy: info.origin_y,
        };
        let b64 = base64::engine::general_purpose::STANDARD.encode(&reply.payload);
        Ok(Content::image(b64, info.mime))
    }

    /// Region screenshot at up to native resolution. Does not touch the view.
    async fn shot_region(&self, region: Rect, max_side: u32) -> Result<Content, ErrorData> {
        let reply = self
            .blocking(move |c| {
                c.call(&Request::Screenshot {
                    max_side: Some(max_side),
                    format: ImageFormat::Png,
                    quality: None,
                    cursor: Some(true),
                    region: Some(region),
                })
            })
            .await?;
        let info: ScreenshotInfo = reply.data().map_err(err)?;
        let b64 = base64::engine::general_purpose::STANDARD.encode(&reply.payload);
        Ok(Content::image(b64, info.mime))
    }

    fn sample(client: &Client, region: Option<Rect>) -> anyhow::Result<Vec<u8>> {
        let reply = client.call(&Request::Screenshot {
            max_side: Some(320),
            format: ImageFormat::Png,
            quality: None,
            cursor: Some(false),
            region,
        })?;
        Ok(image::load_from_memory(&reply.payload)?.into_rgba8().into_raw())
    }

    fn changed_ratio(a: &[u8], b: &[u8]) -> f32 {
        if a.len() != b.len() || a.is_empty() {
            return 1.0;
        }
        let n = a.len() / 4;
        let mut diff = 0usize;
        for i in 0..n {
            let o = i * 4;
            let d = (a[o] as i32 - b[o] as i32).abs()
                + (a[o + 1] as i32 - b[o + 1] as i32).abs()
                + (a[o + 2] as i32 - b[o + 2] as i32).abs();
            if d > 48 {
                diff += 1;
            }
        }
        diff as f32 / n as f32
    }

    async fn wait_stable_inner(
        &self,
        region: Option<Rect>,
        quiet_ms: u32,
        timeout_ms: u32,
        threshold: f32,
    ) -> Result<(bool, f32, f32), ErrorData> {
        self.blocking(move |c| {
            let t0 = Instant::now();
            let mut prev = Self::sample(c, region)?;
            let mut quiet_since = Instant::now();
            let mut last_ratio = 0.0;
            loop {
                std::thread::sleep(Duration::from_millis(120));
                let cur = Self::sample(c, region)?;
                last_ratio = Self::changed_ratio(&prev, &cur);
                if last_ratio > threshold {
                    quiet_since = Instant::now();
                }
                prev = cur;
                if quiet_since.elapsed() >= Duration::from_millis(quiet_ms as u64) {
                    return Ok((true, t0.elapsed().as_secs_f32(), last_ratio));
                }
                if t0.elapsed() >= Duration::from_millis(timeout_ms as u64) {
                    return Ok((false, t0.elapsed().as_secs_f32(), last_ratio));
                }
            }
        })
        .await
    }

    /// Tail of every action: `OK`, or with `settle`, a settled screenshot.
    async fn done(&self, what: String, settle: bool) -> Result<CallToolResult, ErrorData> {
        self.mark(what);
        if !settle {
            return ok();
        }
        let (stable, secs, _) = self.wait_stable_inner(None, 600, 8000, 0.002).await?;
        Ok(CallToolResult::success(vec![
            Content::text(if stable {
                format!("OK; screen settled after {secs:.1}s")
            } else {
                format!("OK; screen still changing after {secs:.1}s")
            }),
            self.shot_full().await?,
        ]))
    }

    // ----- primitive actions shared by the flat tools and computer_batch -----

    async fn do_click(&self, coord: [i32; 2], button: Button, count: u32, text: &Option<String>) -> Result<(), ErrorData> {
        let (x, y) = self.to_scanout(coord[0], coord[1]);
        let m = modifiers(text);
        self.blocking(move |c| c.call(&Request::Click { x, y, button, count, modifiers: m }))
            .await
            .map(|_| ())
    }

    async fn do_move(&self, coord: [i32; 2]) -> Result<(), ErrorData> {
        let (x, y) = self.to_scanout(coord[0], coord[1]);
        self.blocking(move |c| c.call(&Request::PointerMove { x, y })).await.map(|_| ())
    }

    async fn do_drag(&self, start: Option<[i32; 2]>, end: [i32; 2], text: &Option<String>) -> Result<(), ErrorData> {
        let (fx, fy) = match start {
            Some(s) => self.to_scanout(s[0], s[1]),
            None => self.cursor_scanout().await?,
        };
        let (tx, ty) = self.to_scanout(end[0], end[1]);
        let m = modifiers(text);
        self.blocking(move |c| {
            c.call(&Request::Drag {
                from_x: fx,
                from_y: fy,
                to_x: tx,
                to_y: ty,
                button: Button::Left,
                steps: 16,
                modifiers: m,
            })
        })
        .await
        .map(|_| ())
    }

    async fn do_scroll(&self, coord: Option<[i32; 2]>, dir: &str, amount: u32, text: &Option<String>) -> Result<(), ErrorData> {
        let (x, y) = match coord {
            Some(c) => self.to_scanout(c[0], c[1]),
            None => self.cursor_scanout().await?,
        };
        let n = amount.min(100) as i32;
        let (dx, dy) = match dir.to_ascii_lowercase().as_str() {
            "up" => (0, -n),
            "down" => (0, n),
            "left" => (-n, 0),
            "right" => (n, 0),
            other => return Err(bad(format!("scroll_direction must be up|down|left|right, got {other}"))),
        };
        let m = modifiers(text);
        self.blocking(move |c| c.call(&Request::Scroll { x, y, dx, dy, modifiers: m }))
            .await
            .map(|_| ())
    }

    async fn do_key(&self, text: &str, repeat: u32) -> Result<(), ErrorData> {
        let combo = text.to_string();
        let n = repeat.clamp(1, 100);
        self.blocking(move |c| {
            for _ in 0..n {
                c.call(&Request::KeyCombo { keys: vec![combo.clone()] })?;
            }
            Ok(())
        })
        .await
    }

    async fn do_hold_key(&self, text: &str, duration: f32) -> Result<(), ErrorData> {
        let combo = text.to_string();
        let secs = duration.clamp(0.0, 100.0);
        self.blocking(move |c| {
            c.with_timeout(Duration::from_secs_f32(secs + 20.0))
                .call(&Request::KeyHold { keys: vec![combo], seconds: secs })
        })
        .await
        .map(|_| ())
    }

    async fn do_type(&self, text: &str) -> Result<(), ErrorData> {
        let t = text.to_string();
        self.blocking(move |c| crate::type_text(c, &t)).await
    }

    async fn do_button(&self, button: Button, down: bool) -> Result<(), ErrorData> {
        self.blocking(move |c| c.call(&Request::PointerButton { button, down }))
            .await
            .map(|_| ())
    }

    async fn do_cursor_position(&self) -> Result<String, ErrorData> {
        // The hardware cursor plane reflects an injected move only after the
        // next compositor frame (measured: stale at 0 ms, correct at 50 ms).
        tokio::time::sleep(Duration::from_millis(60)).await;
        let (x, y) = self.cursor_scanout().await?;
        let v = *self.view.lock().unwrap();
        Ok(format!(
            "X={}, Y={}",
            ((x - v.ox) as f32 / v.scale).round() as i32,
            ((y - v.oy) as f32 / v.scale).round() as i32
        ))
    }

    async fn do_zoom(&self, region: [i32; 4]) -> Result<Content, ErrorData> {
        let [x0, y0, x1, y1] = region;
        let (w, h) = (x1 - x0, y1 - y0);
        if w <= 0 || h <= 0 {
            return Err(bad("zoom region must be [x0, y0, x1, y1] with x1 > x0 and y1 > y0"));
        }
        let rect = self.rect_to_scanout(x0, y0, w, h);
        self.shot_region(rect, self.max_side).await
    }

    fn rec_path_or_last(&self, p: Option<String>) -> Result<PathBuf, ErrorData> {
        if let Some(p) = p {
            return Ok(PathBuf::from(p));
        }
        self.last_rec
            .lock()
            .unwrap()
            .clone()
            .ok_or_else(|| bad("no recording in this session; pass `path` or call record_stop first"))
    }

    /// Run one batch action. Returns the content to append.
    async fn run_batch_action(&self, a: &BatchAction) -> Result<Vec<Content>, ErrorData> {
        let need = |v: &Option<[i32; 2]>, what: &str| -> Result<[i32; 2], ErrorData> {
            v.ok_or_else(|| bad(format!("{what} needs `coordinate`")))
        };
        let need_text = |v: &Option<String>, what: &str| -> Result<String, ErrorData> {
            v.clone().ok_or_else(|| bad(format!("{what} needs `text`")))
        };
        let ok_text = |s: &str| vec![Content::text(s.to_string())];
        let what = a.action.as_str();
        let out = match what {
            "screenshot" => vec![self.shot_full().await?],
            "zoom" => vec![self.do_zoom(a.region.ok_or_else(|| bad("zoom needs `region`"))?).await?],
            "cursor_position" => ok_text(&self.do_cursor_position().await?),
            "mouse_move" => {
                self.do_move(need(&a.coordinate, what)?).await?;
                ok_text("OK")
            }
            "left_click" | "right_click" | "middle_click" | "double_click" | "triple_click" => {
                let (button, count) = match what {
                    "right_click" => (Button::Right, 1),
                    "middle_click" => (Button::Middle, 1),
                    "double_click" => (Button::Left, 2),
                    "triple_click" => (Button::Left, 3),
                    _ => (Button::Left, 1),
                };
                self.do_click(need(&a.coordinate, what)?, button, count, &a.text).await?;
                ok_text("OK")
            }
            "left_click_drag" => {
                self.do_drag(a.start_coordinate, need(&a.coordinate, what)?, &a.text).await?;
                ok_text("OK")
            }
            "left_mouse_down" => {
                self.do_button(Button::Left, true).await?;
                ok_text("OK")
            }
            "left_mouse_up" => {
                self.do_button(Button::Left, false).await?;
                ok_text("OK")
            }
            "scroll" => {
                let dir = a.scroll_direction.clone().ok_or_else(|| bad("scroll needs `scroll_direction`"))?;
                self.do_scroll(a.coordinate, &dir, a.scroll_amount.unwrap_or(3), &a.text).await?;
                ok_text("OK")
            }
            "type" => {
                self.do_type(&need_text(&a.text, what)?).await?;
                ok_text("OK")
            }
            "key" => {
                self.do_key(&need_text(&a.text, what)?, a.repeat.unwrap_or(1)).await?;
                ok_text("OK")
            }
            "hold_key" => {
                self.do_hold_key(&need_text(&a.text, what)?, a.duration.unwrap_or(1.0)).await?;
                ok_text("OK")
            }
            "wait" => {
                tokio::time::sleep(Duration::from_secs_f32(a.duration.unwrap_or(1.0).clamp(0.0, 100.0))).await;
                vec![self.shot_full().await?]
            }
            other => return Err(bad(format!("unknown action {other}"))),
        };
        self.mark(format!("batch {what}"));
        Ok(out)
    }
}

// ---------- tools ----------

#[tool_router]
impl CuaServer {
    pub fn new(client: Client, max_side: u32) -> Self {
        Self {
            client: Arc::new(client),
            view: Arc::new(Mutex::new(View { scale: 1.0, ox: 0, oy: 0 })),
            rec: Arc::new(Mutex::new(None)),
            last_rec: Arc::new(Mutex::new(None)),
            max_side,
            tool_router: Self::tool_router(),
        }
    }

    // ----- Claude family -----

    #[tool(
        name = "screenshot",
        description = "Take a screenshot of the whole screen, hardware cursor included, straight from the GPU scanout (works on Wayland, X11, lock screen and login screen, no dialog). The returned image is what subsequent click coordinates are relative to."
    )]
    async fn screenshot(&self, Parameters(p): Parameters<ScreenshotParams>) -> Result<CallToolResult, ErrorData> {
        if p.settle.unwrap_or(false) {
            let _ = self.wait_stable_inner(None, 600, 8000, 0.002).await?;
        }
        self.mark("screenshot");
        Ok(CallToolResult::success(vec![self.shot_full().await?]))
    }

    #[tool(
        name = "zoom",
        description = "Magnify a region [x0, y0, x1, y1] of the last screenshot at up to native resolution, to read small text or fine detail. Coordinates you pass to other tools still refer to the full-screen screenshot, never the zoomed image."
    )]
    async fn zoom(&self, Parameters(p): Parameters<ZoomParams>) -> Result<CallToolResult, ErrorData> {
        self.mark(format!("zoom {:?}", p.region));
        Ok(CallToolResult::success(vec![self.do_zoom(p.region).await?]))
    }

    #[tool(name = "left_click", description = "Click the left mouse button at [x, y]. `text` holds modifier keys during the click.")]
    async fn left_click(&self, Parameters(p): Parameters<ClickParams>) -> Result<CallToolResult, ErrorData> {
        self.do_click(p.coordinate, Button::Left, 1, &p.text).await?;
        self.done(format!("left_click {:?}", p.coordinate), p.settle.unwrap_or(false)).await
    }

    #[tool(name = "right_click", description = "Click the right mouse button at [x, y].")]
    async fn right_click(&self, Parameters(p): Parameters<ClickParams>) -> Result<CallToolResult, ErrorData> {
        self.do_click(p.coordinate, Button::Right, 1, &p.text).await?;
        self.done(format!("right_click {:?}", p.coordinate), p.settle.unwrap_or(false)).await
    }

    #[tool(name = "middle_click", description = "Click the middle mouse button at [x, y].")]
    async fn middle_click(&self, Parameters(p): Parameters<ClickParams>) -> Result<CallToolResult, ErrorData> {
        self.do_click(p.coordinate, Button::Middle, 1, &p.text).await?;
        self.done(format!("middle_click {:?}", p.coordinate), p.settle.unwrap_or(false)).await
    }

    #[tool(name = "double_click", description = "Double-click the left mouse button at [x, y].")]
    async fn double_click(&self, Parameters(p): Parameters<ClickParams>) -> Result<CallToolResult, ErrorData> {
        self.do_click(p.coordinate, Button::Left, 2, &p.text).await?;
        self.done(format!("double_click {:?}", p.coordinate), p.settle.unwrap_or(false)).await
    }

    #[tool(name = "triple_click", description = "Triple-click the left mouse button at [x, y], e.g. to select a line.")]
    async fn triple_click(&self, Parameters(p): Parameters<ClickParams>) -> Result<CallToolResult, ErrorData> {
        self.do_click(p.coordinate, Button::Left, 3, &p.text).await?;
        self.done(format!("triple_click {:?}", p.coordinate), p.settle.unwrap_or(false)).await
    }

    #[tool(
        name = "left_click_drag",
        description = "Click and drag the left mouse button from `start_coordinate` (or the current cursor) to `coordinate`."
    )]
    async fn left_click_drag(&self, Parameters(p): Parameters<DragParams>) -> Result<CallToolResult, ErrorData> {
        self.do_drag(p.start_coordinate, p.coordinate, &p.text).await?;
        self.done(format!("left_click_drag {:?} -> {:?}", p.start_coordinate, p.coordinate), p.settle.unwrap_or(false))
            .await
    }

    #[tool(name = "mouse_move", description = "Move the cursor to [x, y] without clicking.")]
    async fn mouse_move(&self, Parameters(p): Parameters<MoveParams>) -> Result<CallToolResult, ErrorData> {
        self.do_move(p.coordinate).await?;
        self.done(format!("mouse_move {:?}", p.coordinate), false).await
    }

    #[tool(name = "left_mouse_down", description = "Press and hold the left mouse button at the current cursor position. Pair with left_mouse_up.")]
    async fn left_mouse_down(&self) -> Result<CallToolResult, ErrorData> {
        self.do_button(Button::Left, true).await?;
        self.done("left_mouse_down".into(), false).await
    }

    #[tool(name = "left_mouse_up", description = "Release the left mouse button.")]
    async fn left_mouse_up(&self) -> Result<CallToolResult, ErrorData> {
        self.do_button(Button::Left, false).await?;
        self.done("left_mouse_up".into(), false).await
    }

    #[tool(name = "cursor_position", description = "Get the current cursor position in the last screenshot's pixels.")]
    async fn cursor_position(&self) -> Result<CallToolResult, ErrorData> {
        let s = self.do_cursor_position().await?;
        Ok(CallToolResult::success(vec![Content::text(s)]))
    }

    #[tool(
        name = "scroll",
        description = "Scroll at [x, y] (or the current cursor) in `scroll_direction` by `scroll_amount` wheel ticks. `text` holds modifier keys while scrolling."
    )]
    async fn scroll(&self, Parameters(p): Parameters<ScrollParams>) -> Result<CallToolResult, ErrorData> {
        self.do_scroll(p.coordinate, &p.scroll_direction, p.scroll_amount, &p.text).await?;
        self.done(
            format!("scroll {} x{} at {:?}", p.scroll_direction, p.scroll_amount, p.coordinate),
            p.settle.unwrap_or(false),
        )
        .await
    }

    #[tool(
        name = "type",
        description = "Type a string of text into the focused control. ASCII goes through the virtual keyboard; anything else is pasted through the clipboard."
    )]
    async fn type_text(&self, Parameters(p): Parameters<TypeParams>) -> Result<CallToolResult, ErrorData> {
        self.do_type(&p.text).await?;
        let shown: String = p.text.chars().take(40).collect();
        self.done(format!("type {shown:?}"), p.settle.unwrap_or(false)).await
    }

    #[tool(
        name = "key",
        description = "Press a key or key combination using xdotool syntax: \"Return\", \"ctrl+s\", \"alt+Tab\", \"Page_Down\", \"super\". Fully released afterwards. `repeat` presses it several times."
    )]
    async fn key(&self, Parameters(p): Parameters<KeyParams>) -> Result<CallToolResult, ErrorData> {
        self.do_key(&p.text, p.repeat.unwrap_or(1)).await?;
        self.done(format!("key {}", p.text), p.settle.unwrap_or(false)).await
    }

    #[tool(name = "hold_key", description = "Hold a key or chord down for `duration` seconds, then release it.")]
    async fn hold_key(&self, Parameters(p): Parameters<HoldKeyParams>) -> Result<CallToolResult, ErrorData> {
        self.do_hold_key(&p.text, p.duration).await?;
        self.done(format!("hold_key {} {:.1}s", p.text, p.duration), false).await
    }

    #[tool(name = "wait", description = "Wait for `duration` seconds, then take a screenshot.")]
    async fn wait(&self, Parameters(p): Parameters<WaitParams>) -> Result<CallToolResult, ErrorData> {
        let secs = p.duration.clamp(0.0, 100.0);
        tokio::time::sleep(Duration::from_secs_f32(secs)).await;
        self.mark(format!("wait {secs}s"));
        Ok(CallToolResult::success(vec![self.shot_full().await?]))
    }

    #[tool(
        name = "computer_batch",
        description = "Run several computer actions in one call, in order. Each item has the legacy single-tool shape: action plus coordinate / start_coordinate / text / scroll_direction / scroll_amount / duration / repeat / region. Stops at the first failure; later items are reported as not executed. End with a screenshot action to see the result."
    )]
    async fn computer_batch(&self, Parameters(p): Parameters<BatchParams>) -> Result<CallToolResult, ErrorData> {
        let mut content = Vec::new();
        let mut failed = false;
        for (i, a) in p.actions.iter().enumerate() {
            if failed {
                content.push(Content::text(format!("[{i}] {}: {NOT_EXECUTED}", a.action)));
                continue;
            }
            match self.run_batch_action(a).await {
                Ok(items) => {
                    content.push(Content::text(format!("[{i}] {}", a.action)));
                    content.extend(items);
                }
                Err(e) => {
                    failed = true;
                    content.push(Content::text(format!("[{i}] {} failed: {}", a.action, e.message)));
                }
            }
        }
        if failed {
            Ok(CallToolResult::error(content))
        } else {
            Ok(CallToolResult::success(content))
        }
    }

    // ----- extras -----

    #[tool(
        name = "wait_for_stable",
        description = "Block until the screen (or a region) has stopped changing for quiet_ms, then take a screenshot. Use after an action that triggers loading or animation instead of guessing a wait. Reports whether it settled or timed out."
    )]
    async fn wait_for_stable(&self, Parameters(p): Parameters<WaitStableParams>) -> Result<CallToolResult, ErrorData> {
        let region = self.region_xywh(p.region);
        let (stable, secs, ratio) = self
            .wait_stable_inner(region, p.quiet_ms.unwrap_or(600), p.timeout_ms.unwrap_or(8000), p.threshold.unwrap_or(0.002))
            .await?;
        self.mark(format!("wait_for_stable -> {}", if stable { "settled" } else { "timeout" }));
        Ok(CallToolResult::success(vec![
            Content::text(format!(
                "{} after {:.1}s (last change ratio {:.4})",
                if stable { "settled" } else { "timed out, still changing" },
                secs,
                ratio
            )),
            self.shot_full().await?,
        ]))
    }

    #[tool(
        name = "wait_for_change",
        description = "Block until the screen (or a region) differs from how it looks now by at least threshold, then take a screenshot. Confirms an action had an effect without polling."
    )]
    async fn wait_for_change(&self, Parameters(p): Parameters<WaitChangeParams>) -> Result<CallToolResult, ErrorData> {
        let region = self.region_xywh(p.region);
        let timeout = p.timeout_ms.unwrap_or(8000);
        let threshold = p.threshold.unwrap_or(0.01);
        let (changed, secs, ratio) = self
            .blocking(move |c| {
                let t0 = Instant::now();
                let base = Self::sample(c, region)?;
                loop {
                    std::thread::sleep(Duration::from_millis(120));
                    let cur = Self::sample(c, region)?;
                    let r = Self::changed_ratio(&base, &cur);
                    if r >= threshold {
                        return Ok((true, t0.elapsed().as_secs_f32(), r));
                    }
                    if t0.elapsed() >= Duration::from_millis(timeout as u64) {
                        return Ok((false, t0.elapsed().as_secs_f32(), r));
                    }
                }
            })
            .await?;
        self.mark(format!("wait_for_change -> {}", if changed { "changed" } else { "timeout" }));
        Ok(CallToolResult::success(vec![
            Content::text(format!(
                "{} after {:.1}s (change ratio {:.4})",
                if changed { "changed" } else { "timed out, no change" },
                secs,
                ratio
            )),
            self.shot_full().await?,
        ]))
    }

    #[tool(
        name = "record_start",
        description = "Start recording the screen to an H.264 MP4 (hardware encoder when available). Every tool call you make while recording is stamped into a timeline. Stop with record_stop; it returns a manifest and a contact sheet, never the video."
    )]
    async fn record_start(&self, Parameters(p): Parameters<RecordStartParams>) -> Result<CallToolResult, ErrorData> {
        let name = p.name.clone();
        let reply = self
            .blocking(move |c| {
                c.call(&Request::RecordStart {
                    name,
                    fps: p.fps,
                    max_side: p.max_side,
                    bitrate_kbps: None,
                    max_seconds: p.max_seconds,
                    cursor: Some(true),
                })
            })
            .await?;
        let info: RecordInfo = reply.data().map_err(err)?;
        let path = PathBuf::from(info.path.clone().unwrap_or_default());
        *self.rec.lock().unwrap() = Some(RecState {
            started: Instant::now(),
            path: path.clone(),
            timeline: vec![(0.0, "record_start".into())],
        });
        Ok(CallToolResult::success(vec![Content::text(format!(
            "recording {} at {}x{} {} fps with {}",
            path.display(),
            info.width,
            info.height,
            info.fps,
            info.codec.unwrap_or_default()
        ))]))
    }

    #[tool(name = "record_mark", description = "Stamp a note into the recording timeline at the current time.")]
    async fn record_mark(&self, Parameters(p): Parameters<RecordMarkParams>) -> Result<CallToolResult, ErrorData> {
        if self.rec.lock().unwrap().is_none() {
            return Err(bad("not recording"));
        }
        self.mark(format!("mark: {}", p.note));
        ok()
    }

    #[tool(name = "record_status", description = "Whether a recording is active, and its frame count and duration so far.")]
    async fn record_status(&self) -> Result<CallToolResult, ErrorData> {
        let reply = self.blocking(|c| c.call(&Request::RecordStatus)).await?;
        let info: RecordInfo = reply.data().map_err(err)?;
        Ok(CallToolResult::success(vec![Content::text(serde_json::to_string_pretty(&info).unwrap_or_default())]))
    }

    #[tool(
        name = "record_stop",
        description = "Stop the recording. Returns the MP4 path, duration, the numbered timeline of every action taken while recording (for record_frames `between`), and a contact sheet of evenly spaced frames."
    )]
    async fn record_stop(&self, Parameters(p): Parameters<RecordStopParams>) -> Result<CallToolResult, ErrorData> {
        let reply = self.blocking(|c| c.call(&Request::RecordStop)).await?;
        let info: RecordInfo = reply.data().map_err(err)?;
        let state = self.rec.lock().unwrap().take();
        let path = PathBuf::from(info.path.clone().unwrap_or_default());
        *self.last_rec.lock().unwrap() = Some(path.clone());
        let mut lines = vec![format!(
            "recording {} | {:.1}s, {} frames at {} fps, {}x{}, {} | finalized={} {}",
            path.display(),
            info.seconds,
            info.frames,
            info.fps,
            info.width,
            info.height,
            info.codec.clone().unwrap_or_default(),
            info.finalized.unwrap_or(false),
            info.error.clone().unwrap_or_default()
        )];
        if let Some(s) = &state {
            lines.push("timeline:".into());
            for (i, (t, what)) in s.timeline.iter().enumerate() {
                lines.push(format!("  [{i}] {t:7.2}s  {what}"));
            }
            let _ = video::write_timeline(&path, &s.timeline);
        }
        let mut content = vec![Content::text(lines.join("\n"))];
        if p.sheet.unwrap_or(true) && info.finalized.unwrap_or(false) {
            let tiles = p.tiles.unwrap_or(6).clamp(1, 16);
            let dur = info.seconds.max(0.1);
            let p2 = path.clone();
            match self.blocking(move |_| video::contact_sheet(&p2, dur, tiles, 480)).await {
                Ok((png, times)) => {
                    content.push(Content::text(format!(
                        "contact sheet, tiles left to right, top to bottom, at seconds: {}",
                        times.iter().map(|t| format!("{t:.1}")).collect::<Vec<_>>().join(", ")
                    )));
                    content.push(Content::image(base64::engine::general_purpose::STANDARD.encode(&png), "image/png"));
                }
                Err(e) => content.push(Content::text(format!("contact sheet unavailable: {}", e.message))),
            }
        }
        Ok(CallToolResult::success(content))
    }

    #[tool(
        name = "record_frames",
        description = "Extract still frames from a recording: at exact seconds (`at`), every N seconds (`every`), where the picture changed (`scene`, 0..1), or between two timeline entries (`between`). Returns up to `max` images with their timestamps."
    )]
    async fn record_frames(&self, Parameters(p): Parameters<RecordFramesParams>) -> Result<CallToolResult, ErrorData> {
        let path = self.rec_path_or_last(p.path.clone())?;
        let max = p.max.unwrap_or(8).clamp(1, 24) as usize;
        let side = p.max_side.unwrap_or(800).clamp(64, 2000);
        let (t0, t1) = match p.between {
            Some([a, b]) => {
                let tl = video::read_timeline(&path).map_err(err)?;
                let get = |i: u32| tl.get(i as usize).map(|(t, _)| *t).ok_or_else(|| bad(format!("no timeline entry {i}")));
                (Some(get(a)?), Some(get(b)?))
            }
            None => (None, None),
        };
        let p2 = path.clone();
        let frames = self
            .blocking(move |_| {
                let dur = video::duration(&p2)?;
                let (a, b) = (t0.unwrap_or(0.0), t1.unwrap_or(dur));
                let times: Vec<f32> = if let Some(at) = p.at.clone() {
                    at
                } else if let Some(th) = p.scene {
                    let mut v = video::scene_changes(&p2, th, a, b)?;
                    if v.is_empty() {
                        v = vec![a, b];
                    }
                    v
                } else {
                    let step = p.every.unwrap_or(((b - a) / (max as f32 - 1.0).max(1.0)).max(0.1));
                    let mut v = Vec::new();
                    let mut t = a;
                    while t <= b + 1e-3 {
                        v.push(t);
                        t += step;
                    }
                    v
                };
                video::frames_at(&p2, &video::thin(times, max), side)
            })
            .await?;
        let mut content = vec![Content::text(format!(
            "{} frames from {} at seconds: {}",
            frames.len(),
            path.display(),
            frames.iter().map(|(t, _)| format!("{t:.2}")).collect::<Vec<_>>().join(", ")
        ))];
        for (t, png) in frames {
            content.push(Content::text(format!("t={t:.2}s")));
            content.push(Content::image(base64::engine::general_purpose::STANDARD.encode(&png), "image/png"));
        }
        Ok(CallToolResult::success(content))
    }

    #[tool(name = "doctor", description = "Report whether capture, input, clipboard paste and recording are available and why not.")]
    async fn doctor(&self) -> Result<CallToolResult, ErrorData> {
        let d = self.blocking(|c| Ok(crate::doctor(c))).await?;
        Ok(CallToolResult::success(vec![Content::text(serde_json::to_string_pretty(&d).unwrap_or_default())]))
    }
}

#[tool_handler]
impl ServerHandler for CuaServer {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::new("kmscua", env!("CARGO_PKG_VERSION")))
            .with_instructions(
                "Computer use for Linux, below the compositor. Take a screenshot first; every coordinate you pass is in that image's pixels, and zoom never changes that. Actions answer OK and never leave a key or button held (except left_mouse_down). Use wait_for_stable after actions that trigger loading, or pass settle=true on the action. Use record_start/record_stop around a multi-step task when you need to see what happened between actions; ask for stills with record_frames.",
            )
    }
}

pub async fn serve(client: Client, max_side: Option<u32>) -> anyhow::Result<()> {
    let server = CuaServer::new(client, max_side.unwrap_or(DEFAULT_MAX_SIDE));
    let running = server.serve(rmcp::transport::stdio()).await?;
    running.waiting().await?;
    Ok(())
}
