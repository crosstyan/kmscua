//! Wire protocol between `cua` (unprivileged, in the user session) and `cuad`
//! (root, owns the DRM scanout and the uinput devices).
//!
//! Framing, both directions: `u32` little-endian length, then that many bytes
//! of JSON. A response may carry a binary payload after its JSON header; the
//! header's `bytes` field says how many bytes follow.
//!
//! Every coordinate in this protocol is a scanout pixel: the same space the
//! screenshot is in. There is no logical/physical scale anywhere.

use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};

pub const DEFAULT_SOCKET: &str = "/run/kmscua/cuad.sock";
pub const MAX_FRAME: usize = 64 * 1024 * 1024;

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "lowercase")]
pub enum Button {
    #[default]
    Left,
    Right,
    Middle,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "lowercase")]
pub enum ImageFormat {
    #[default]
    Png,
    Jpeg,
}

impl ImageFormat {
    pub fn mime(self) -> &'static str {
        match self {
            ImageFormat::Png => "image/png",
            ImageFormat::Jpeg => "image/jpeg",
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub struct Rect {
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum Request {
    Ping,
    /// Daemon status: backend, displays, uinput devices, uptime.
    Status,
    Displays,
    /// One frame of the scanout with the hardware cursor composited in.
    Screenshot {
        /// Upper bound on the longest side of the returned image. The
        /// daemon downscales by an integer factor, so 3840 -> 1920 for 1920
        /// and 3840 -> 1280 for 1568. None = native.
        #[serde(default)]
        max_side: Option<u32>,
        #[serde(default)]
        format: ImageFormat,
        /// JPEG quality 1-100 (ignored for PNG).
        #[serde(default)]
        quality: Option<u8>,
        /// Composite the cursor (default true).
        #[serde(default)]
        cursor: Option<bool>,
        /// Crop to this scanout rect before resizing.
        #[serde(default)]
        region: Option<Rect>,
    },
    /// Where the hardware cursor is right now, in scanout pixels.
    Cursor,
    PointerMove {
        x: i32,
        y: i32,
    },
    PointerButton {
        button: Button,
        down: bool,
    },
    Click {
        x: i32,
        y: i32,
        #[serde(default)]
        button: Button,
        #[serde(default = "one")]
        count: u32,
        /// Key names held for the duration of the click, e.g. ["ctrl","shift"].
        #[serde(default)]
        modifiers: Vec<String>,
    },
    Drag {
        from_x: i32,
        from_y: i32,
        to_x: i32,
        to_y: i32,
        #[serde(default)]
        button: Button,
        /// Intermediate motion events between the endpoints.
        #[serde(default = "drag_steps")]
        steps: u32,
        #[serde(default)]
        modifiers: Vec<String>,
    },
    /// Wheel detents. `dy > 0` scrolls down, `dx > 0` scrolls right.
    Scroll {
        x: i32,
        y: i32,
        #[serde(default)]
        dx: i32,
        #[serde(default)]
        dy: i32,
        #[serde(default)]
        modifiers: Vec<String>,
    },
    /// Raw evdev key code (`KEY_*` from linux/input-event-codes.h).
    Key {
        code: u16,
        down: bool,
    },
    /// Chord by name: `["ctrl", "shift", "t"]`. Pressed in order, released in
    /// reverse, always fully released before the reply.
    KeyCombo {
        keys: Vec<String>,
    },
    /// Hold a chord down for `seconds` (max 100), then release it.
    KeyHold {
        keys: Vec<String>,
        seconds: f32,
    },
    /// Type text through the virtual keyboard. ASCII only in this version;
    /// callers paste anything else through the clipboard.
    Type {
        text: String,
        /// Delay between key events in milliseconds.
        #[serde(default)]
        delay_ms: Option<u32>,
    },
    /// Nudge the pointer by one pixel and back, to wake a blanked output.
    Wake,
    /// Start recording the scanout to an MP4 in the daemon's recording dir.
    RecordStart {
        /// Base name without extension; default is a timestamp.
        #[serde(default)]
        name: Option<String>,
        /// Frames per second, default 15.
        #[serde(default)]
        fps: Option<u32>,
        /// Longest side of the encoded video, default 1920 (integer box
        /// downscale of the scanout, so 3840 -> 1920).
        #[serde(default)]
        max_side: Option<u32>,
        /// Video bitrate in kbit/s, default 6000.
        #[serde(default)]
        bitrate_kbps: Option<u32>,
        /// Stop automatically after this many seconds, default 600.
        #[serde(default)]
        max_seconds: Option<u32>,
        /// Composite the cursor, default true.
        #[serde(default)]
        cursor: Option<bool>,
    },
    /// Stop the recording and finalize the file.
    RecordStop,
    RecordStatus,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RecordInfo {
    pub recording: bool,
    pub path: Option<String>,
    pub codec: Option<String>,
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    pub frames: u64,
    pub dropped: u64,
    pub seconds: f32,
    /// Set once stopped: whether the encoder exited cleanly and the file is playable.
    pub finalized: Option<bool>,
    pub error: Option<String>,
}

fn one() -> u32 {
    1
}
fn drag_steps() -> u32 {
    12
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DisplayInfo {
    pub name: String,
    pub crtc_id: u32,
    pub connector_id: u32,
    pub x: u32,
    pub y: u32,
    pub width: u32,
    pub height: u32,
    pub refresh_hz: u32,
    pub active: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CursorInfo {
    pub visible: bool,
    pub x: i32,
    pub y: i32,
    pub hot_x: i32,
    pub hot_y: i32,
    pub width: u32,
    pub height: u32,
}

/// Header of a screenshot reply; the encoded image follows as the payload.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScreenshotInfo {
    /// Size of the returned image.
    pub width: u32,
    pub height: u32,
    /// Size of the scanout region the image was taken from. Multiply an image
    /// coordinate by `scale` to get a scanout coordinate.
    pub source_width: u32,
    pub source_height: u32,
    pub scale: f32,
    /// Scanout offset of the region (0,0 for a full-screen shot).
    pub origin_x: i32,
    pub origin_y: i32,
    pub mime: String,
    pub cursor: Option<CursorInfo>,
    pub grab_ms: f32,
    pub encode_ms: f32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Status {
    pub version: String,
    pub uptime_s: u64,
    pub gpu_driver: Option<String>,
    pub displays: Vec<DisplayInfo>,
    /// Union rect of all active displays; the uinput ABS range.
    pub desktop: Rect,
    pub pointer_device: String,
    pub keyboard_device: String,
    pub cursor_supported: bool,
    pub requests_served: u64,
    /// Encoder the recorder will use, None when recording is disabled or no
    /// GStreamer encoder was found.
    pub record_codec: Option<String>,
    pub record_dir: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Header {
    pub ok: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(default)]
    pub data: serde_json::Value,
    #[serde(default)]
    pub bytes: u64,
}

pub fn write_frame<W: Write>(w: &mut W, json: &[u8], payload: &[u8]) -> Result<()> {
    let len = u32::try_from(json.len()).context("frame too large")?;
    w.write_all(&len.to_le_bytes())?;
    w.write_all(json)?;
    if !payload.is_empty() {
        w.write_all(payload)?;
    }
    w.flush()?;
    Ok(())
}

pub fn read_json_frame<R: Read>(r: &mut R) -> Result<Vec<u8>> {
    let mut len = [0u8; 4];
    r.read_exact(&mut len).context("read frame length")?;
    let len = u32::from_le_bytes(len) as usize;
    if len > MAX_FRAME {
        bail!("frame of {len} bytes exceeds limit");
    }
    let mut buf = vec![0u8; len];
    r.read_exact(&mut buf).context("read frame body")?;
    Ok(buf)
}

/// A reply from the daemon: decoded header plus the raw payload (if any).
#[derive(Debug)]
pub struct Reply {
    pub header: Header,
    pub payload: Vec<u8>,
}

impl Reply {
    pub fn into_result(self) -> Result<Reply> {
        if self.header.ok {
            Ok(self)
        } else {
            bail!(
                "{}",
                self.header
                    .error
                    .unwrap_or_else(|| "daemon returned an error without a message".into())
            )
        }
    }

    pub fn data<T: serde::de::DeserializeOwned>(&self) -> Result<T> {
        serde_json::from_value(self.header.data.clone()).context("decode reply data")
    }
}

/// Blocking client. One connection per request keeps the daemon stateless
/// per peer and lets it enforce per-request atomicity.
pub struct Client {
    socket: std::path::PathBuf,
    timeout: Duration,
}

impl Clone for Client {
    fn clone(&self) -> Self {
        Self { socket: self.socket.clone(), timeout: self.timeout }
    }
}

impl Client {
    pub fn new(socket: impl AsRef<Path>) -> Self {
        Self {
            socket: socket.as_ref().to_path_buf(),
            timeout: Duration::from_secs(20),
        }
    }

    pub fn from_env() -> Self {
        let path = std::env::var("KMSCUA_SOCKET").unwrap_or_else(|_| DEFAULT_SOCKET.to_string());
        Self::new(path)
    }

    pub fn socket_path(&self) -> &Path {
        &self.socket
    }

    pub fn with_timeout(&self, timeout: Duration) -> Self {
        Self { socket: self.socket.clone(), timeout }
    }

    pub fn call(&self, req: &Request) -> Result<Reply> {
        let mut stream = UnixStream::connect(&self.socket)
            .with_context(|| format!("connect to cuad at {}", self.socket.display()))?;
        stream.set_read_timeout(Some(self.timeout))?;
        stream.set_write_timeout(Some(self.timeout))?;
        let json = serde_json::to_vec(req)?;
        write_frame(&mut stream, &json, &[])?;
        let header_bytes = read_json_frame(&mut stream)?;
        let header: Header = serde_json::from_slice(&header_bytes).context("decode reply header")?;
        let mut payload = Vec::new();
        if header.bytes > 0 {
            if header.bytes as usize > MAX_FRAME {
                bail!("payload of {} bytes exceeds limit", header.bytes);
            }
            payload.resize(header.bytes as usize, 0);
            stream.read_exact(&mut payload).context("read reply payload")?;
        }
        Reply { header, payload }.into_result()
    }
}
