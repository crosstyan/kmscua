//! kmscua's client layer as a library: the MCP server shaped like the tools
//! Claude models were trained on, the recording readers, and the
//! [`Backend`] seam that lets something other than cuad answer the requests.
//!
//! `cua` serves it with a [`kmscua_proto::Client`]; another crate builds its
//! own [`Backend`] and calls [`serve_stdio`].

#[cfg(feature = "atspi")]
pub mod atspi;
pub mod backend;
pub mod mcp;
pub mod session;
pub mod stills;
pub mod video;

use std::io::Write;
use std::process::{Command, Stdio};

use anyhow::{bail, Result};
use kmscua_proto::{Client, Request, Status};

pub use backend::Backend;
pub use kmscua_proto as proto;
pub use mcp::{serve_stdio, CuaServer, ServeOptions};

/// Type ASCII through the virtual keyboard; anything else through the
/// clipboard and Ctrl+V, which needs the session's Wayland or X socket.
/// A backend that [types Unicode](Backend::types_unicode) gets the whole
/// text in one request.
pub fn type_text(c: &dyn Backend, text: &str) -> Result<()> {
    if text.is_ascii() || c.types_unicode() {
        c.call(&Request::Type {
            text: text.to_string(),
            delay_ms: None,
        })?;
        return Ok(());
    }
    // Mixed text: type ASCII runs, paste the rest, keep order.
    let mut run = String::new();
    let flush_ascii = |run: &mut String| -> Result<()> {
        if !run.is_empty() {
            c.call(&Request::Type {
                text: std::mem::take(run),
                delay_ms: None,
            })?;
        }
        Ok(())
    };
    let mut paste_buf = String::new();
    for ch in text.chars() {
        if ch.is_ascii() {
            if !paste_buf.is_empty() {
                paste(c, &paste_buf)?;
                paste_buf.clear();
            }
            run.push(ch);
        } else {
            flush_ascii(&mut run)?;
            paste_buf.push(ch);
        }
    }
    flush_ascii(&mut run)?;
    if !paste_buf.is_empty() {
        paste(c, &paste_buf)?;
    }
    Ok(())
}

fn paste(c: &dyn Backend, text: &str) -> Result<()> {
    let env = session::hydrate();
    session::apply(&env);
    let tried: &[(&str, &[&str])] = &[
        ("wl-copy", &["--type", "text/plain;charset=utf-8"]),
        ("xclip", &["-selection", "clipboard"]),
        ("xsel", &["--clipboard", "--input"]),
    ];
    let mut last = None;
    for (bin, args) in tried {
        match Command::new(bin)
            .args(*args)
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
        {
            Ok(mut child) => {
                child.stdin.take().unwrap().write_all(text.as_bytes())?;
                let out = child.wait_with_output()?;
                if out.status.success() {
                    std::thread::sleep(std::time::Duration::from_millis(80));
                    c.call(&Request::KeyCombo {
                        keys: vec!["ctrl+v".into()],
                    })?;
                    return Ok(());
                }
                last = Some(format!("{bin}: {}", String::from_utf8_lossy(&out.stderr).trim()));
            }
            Err(e) => last = Some(format!("{bin}: {e}")),
        }
    }
    bail!(
        "cannot paste non-ASCII text: no working clipboard tool ({}); install wl-clipboard or xclip",
        last.unwrap_or_default()
    )
}

#[derive(serde::Serialize)]
pub struct Doctor {
    pub socket: String,
    pub daemon: Option<Status>,
    pub daemon_error: Option<String>,
    pub session_env: std::collections::HashMap<String, String>,
    pub clipboard_tool: Option<String>,
    pub can_screenshot: bool,
    pub can_input: bool,
    pub can_paste_unicode: bool,
    pub can_record: bool,
    pub record_codec: Option<String>,
    pub ffmpeg: bool,
    pub notes: Vec<String>,
}

pub fn doctor(c: &Client) -> Doctor {
    let mut notes = Vec::new();
    let (daemon, daemon_error) = match c.call(&Request::Status).and_then(|r| r.data::<Status>()) {
        Ok(s) => (Some(s), None),
        Err(e) => {
            notes.push(format!(
                "cuad unreachable at {}: start it (systemctl start cuad) and make sure this user is in the socket group",
                c.socket_path().display()
            ));
            (None, Some(format!("{e:#}")))
        }
    };
    let session_env = session::hydrate();
    if !session_env.contains_key("WAYLAND_DISPLAY") && !session_env.contains_key("DISPLAY") {
        notes.push("no graphical session found for this uid; clipboard paste and AT-SPI are unavailable, screenshots and input still work".into());
    }
    let clipboard_tool = ["wl-copy", "xclip", "xsel"]
        .iter()
        .find(|b| which(b))
        .map(|s| s.to_string());
    if clipboard_tool.is_none() {
        notes.push("no clipboard tool: non-ASCII typing will fail (apt install wl-clipboard)".into());
    }
    let can = daemon.is_some();
    let ffmpeg = video::have_ffmpeg();
    let record_codec = daemon.as_ref().and_then(|d| d.record_codec.clone());
    let can_record = matches!(record_codec.as_deref(), Some(c) if c != "detecting");
    if let Some(d) = &daemon {
        match d.record_codec.as_deref() {
            None => notes.push("recording unavailable: no GStreamer H.264 encoder (apt install gstreamer1.0-plugins-good gstreamer1.0-plugins-ugly, or the NVIDIA plugins)".into()),
            Some("detecting") => notes.push("encoder detection still running; retry doctor in a few seconds".into()),
            _ => {}
        }
        if !ffmpeg {
            notes.push("ffmpeg missing: recordings work but contact sheets and frame extraction do not (apt install ffmpeg)".into());
        }
        if !d.cursor_supported {
            notes.push("cursor plane not readable: screenshots will not show the pointer".into());
        }
        if d.displays.iter().all(|d| !d.active) {
            notes.push("no active display: outputs are blanked, try `cua wake`".into());
        }
    }
    Doctor {
        socket: c.socket_path().display().to_string(),
        daemon,
        daemon_error,
        can_screenshot: can,
        can_input: can,
        can_paste_unicode: clipboard_tool.is_some()
            && (session_env.contains_key("WAYLAND_DISPLAY") || session_env.contains_key("DISPLAY")),
        can_record,
        record_codec,
        ffmpeg,
        clipboard_tool,
        session_env,
        notes,
    }
}

fn which(bin: &str) -> bool {
    std::env::var_os("PATH")
        .map(|p| {
            std::env::split_paths(&p).any(|d| d.join(bin).is_file())
        })
        .unwrap_or(false)
}
