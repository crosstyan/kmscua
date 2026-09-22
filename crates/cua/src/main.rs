//! cua: the unprivileged half of kmscua.
//!
//! A CLI for humans and scripts, and an MCP stdio server for agents. Pixels
//! and input go to cuad over its socket. Anything that needs the user's
//! session (clipboard paste for non-ASCII text, AT-SPI later) finds the
//! session environment itself, so it also works from an SSH shell.

mod atspi;
mod mcp;
mod session;
mod video;

use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::io::Write;

use anyhow::{bail, Context, Result};
use clap::{Parser, Subcommand};
use kmscua_proto::{Button, Client, ImageFormat, RecordInfo, Rect, Request, ScreenshotInfo, Status};

#[derive(Parser, Debug)]
#[command(name = "cua", version, about)]
struct Args {
    /// cuad socket (or KMSCUA_SOCKET).
    #[arg(long, global = true)]
    socket: Option<PathBuf>,
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand, Debug)]
enum Cmd {
    /// Check the daemon, the session, and helper tools.
    Doctor,
    /// Daemon status JSON.
    Status,
    /// Connected displays.
    Displays,
    /// Save a screenshot.
    Screenshot {
        #[arg(short, long, default_value = "screenshot.png")]
        out: PathBuf,
        /// Longest side in pixels (default: native).
        #[arg(long)]
        max_side: Option<u32>,
        #[arg(long)]
        jpeg: bool,
        #[arg(long)]
        no_cursor: bool,
        /// Crop: x,y,w,h in scanout pixels.
        #[arg(long, value_parser = parse_rect)]
        region: Option<Rect>,
    },
    /// Where the cursor is.
    Cursor,
    /// Move the pointer.
    Move { x: i32, y: i32 },
    /// Click at a point.
    Click {
        x: i32,
        y: i32,
        #[arg(long, default_value = "left", value_parser = parse_button)]
        button: Button,
        #[arg(long, default_value_t = 1)]
        count: u32,
    },
    /// Drag from one point to another.
    Drag {
        from_x: i32,
        from_y: i32,
        to_x: i32,
        to_y: i32,
        #[arg(long, default_value = "left", value_parser = parse_button)]
        button: Button,
    },
    /// Scroll wheel detents at a point (dy > 0 scrolls down).
    Scroll {
        x: i32,
        y: i32,
        #[arg(long, default_value_t = 0)]
        dx: i32,
        #[arg(long, default_value_t = 0)]
        dy: i32,
    },
    /// Key chord, e.g. "ctrl+shift+t" or "enter".
    Key { combo: String },
    /// Type text (non-ASCII goes through the clipboard).
    Type { text: String },
    /// Wake a blanked display.
    Wake,
    /// Which app, window and element have focus, with the element's text (AT-SPI).
    Focused,
    /// Screen recording (H.264 MP4, hardware encoder when available).
    #[command(subcommand)]
    Record(RecordCmd),
    /// Run the MCP server on stdio.
    Mcp {
        /// Longest side of screenshots sent to the model (default 1920; Claude
        /// accepts up to 2576, 1080p is the documented cost/accuracy balance).
        #[arg(long)]
        max_side: Option<u32>,
    },
}

#[derive(Subcommand, Debug)]
enum RecordCmd {
    /// Start recording.
    Start {
        /// Base name without extension (default: timestamp).
        #[arg(long)]
        name: Option<String>,
        #[arg(long, default_value_t = 15)]
        fps: u32,
        #[arg(long, default_value_t = 1920)]
        max_side: u32,
        #[arg(long, default_value_t = 600)]
        max_seconds: u32,
    },
    /// Stop and finalize; optionally copy the MP4 somewhere.
    Stop {
        #[arg(short, long)]
        out: Option<PathBuf>,
    },
    Status,
    /// Contact sheet PNG of a recording.
    Sheet {
        path: PathBuf,
        #[arg(short, long, default_value = "sheet.png")]
        out: PathBuf,
        #[arg(long, default_value_t = 6)]
        tiles: u32,
    },
    /// Extract frames: --at 1.5,3 | --every 2 | --scene 0.3
    Frames {
        path: PathBuf,
        #[arg(long, value_delimiter = ',')]
        at: Option<Vec<f32>>,
        #[arg(long)]
        every: Option<f32>,
        #[arg(long)]
        scene: Option<f32>,
        #[arg(long, default_value_t = 8)]
        max: usize,
        #[arg(long, default_value = ".")]
        out_dir: PathBuf,
    },
}

fn parse_button(s: &str) -> Result<Button, String> {
    match s.to_ascii_lowercase().as_str() {
        "left" => Ok(Button::Left),
        "right" => Ok(Button::Right),
        "middle" => Ok(Button::Middle),
        _ => Err(format!("unknown button {s}")),
    }
}

fn parse_rect(s: &str) -> Result<Rect, String> {
    let v: Vec<i64> = s
        .split(',')
        .map(|p| p.trim().parse::<i64>().map_err(|e| e.to_string()))
        .collect::<Result<_, _>>()?;
    if v.len() != 4 {
        return Err("expected x,y,w,h".into());
    }
    Ok(Rect {
        x: v[0] as i32,
        y: v[1] as i32,
        width: v[2] as u32,
        height: v[3] as u32,
    })
}

fn client(args: &Args) -> Client {
    match &args.socket {
        Some(p) => Client::new(p),
        None => Client::from_env(),
    }
}

fn main() -> Result<()> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("warn"))
        .target(env_logger::Target::Stderr)
        .init();
    let args = Args::parse();
    let c = client(&args);
    match &args.cmd {
        Cmd::Doctor => {
            let report = doctor(&c);
            println!("{}", serde_json::to_string_pretty(&report)?);
            Ok(())
        }
        Cmd::Status => print_json(&c.call(&Request::Status)?.header.data),
        Cmd::Displays => print_json(&c.call(&Request::Displays)?.header.data),
        Cmd::Screenshot {
            out,
            max_side,
            jpeg,
            no_cursor,
            region,
        } => {
            let reply = c.call(&Request::Screenshot {
                max_side: *max_side,
                format: if *jpeg { ImageFormat::Jpeg } else { ImageFormat::Png },
                quality: None,
                cursor: Some(!*no_cursor),
                region: *region,
            })?;
            let info: ScreenshotInfo = reply.data()?;
            std::fs::write(out, &reply.payload)
                .with_context(|| format!("write {}", out.display()))?;
            eprintln!(
                "{}x{} (source {}x{}, scale {:.2}) grab {:.1} ms encode {:.1} ms -> {}",
                info.width,
                info.height,
                info.source_width,
                info.source_height,
                info.scale,
                info.grab_ms,
                info.encode_ms,
                out.display()
            );
            Ok(())
        }
        Cmd::Cursor => print_json(&c.call(&Request::Cursor)?.header.data),
        Cmd::Move { x, y } => c.call(&Request::PointerMove { x: *x, y: *y }).map(|_| ()),
        Cmd::Click {
            x,
            y,
            button,
            count,
        } => c
            .call(&Request::Click {
                x: *x,
                y: *y,
                button: *button,
                count: *count,
                modifiers: vec![],
            })
            .map(|_| ()),
        Cmd::Drag {
            from_x,
            from_y,
            to_x,
            to_y,
            button,
        } => c
            .call(&Request::Drag {
                from_x: *from_x,
                from_y: *from_y,
                to_x: *to_x,
                to_y: *to_y,
                button: *button,
                steps: 12,
                modifiers: vec![],
            })
            .map(|_| ()),
        Cmd::Scroll { x, y, dx, dy } => c
            .call(&Request::Scroll {
                x: *x,
                y: *y,
                dx: *dx,
                dy: *dy,
                modifiers: vec![],
            })
            .map(|_| ()),
        Cmd::Key { combo } => c
            .call(&Request::KeyCombo {
                keys: vec![combo.clone()],
            })
            .map(|_| ()),
        Cmd::Type { text } => type_text(&c, text),
        Cmd::Wake => c.call(&Request::Wake).map(|_| ()),
        Cmd::Focused => {
            let rt = tokio::runtime::Runtime::new()?;
            let text = rt.block_on(async { atspi::Ui::connect().await?.get_focused().await })?;
            print!("{text}");
            Ok(())
        }
        Cmd::Record(rc) => record_cmd(&c, rc),
        Cmd::Mcp { max_side } => {
            let rt = tokio::runtime::Runtime::new()?;
            rt.block_on(mcp::serve(c, *max_side))
        }
    }
}

fn record_cmd(c: &Client, rc: &RecordCmd) -> Result<()> {
    match rc {
        RecordCmd::Start { name, fps, max_side, max_seconds } => {
            let r = c.call(&Request::RecordStart {
                name: name.clone(),
                fps: Some(*fps),
                max_side: Some(*max_side),
                bitrate_kbps: None,
                max_seconds: Some(*max_seconds),
                cursor: Some(true),
            })?;
            let info: RecordInfo = r.data()?;
            eprintln!(
                "recording {} {}x{} @{} fps with {}",
                info.path.as_deref().unwrap_or("?"),
                info.width,
                info.height,
                info.fps,
                info.codec.unwrap_or_default()
            );
            Ok(())
        }
        RecordCmd::Stop { out } => {
            let r = c.call(&Request::RecordStop)?;
            let info: RecordInfo = r.data()?;
            let path = PathBuf::from(info.path.clone().unwrap_or_default());
            eprintln!(
                "stopped: {:.1}s {} frames ({} dropped) finalized={} {}",
                info.seconds,
                info.frames,
                info.dropped,
                info.finalized.unwrap_or(false),
                info.error.clone().unwrap_or_default()
            );
            if let Some(dest) = out {
                std::fs::copy(&path, dest).with_context(|| format!("copy to {}", dest.display()))?;
                println!("{}", dest.display());
            } else {
                println!("{}", path.display());
            }
            Ok(())
        }
        RecordCmd::Status => print_json(&c.call(&Request::RecordStatus)?.header.data),
        RecordCmd::Sheet { path, out, tiles } => {
            let dur = video::duration(path)?;
            let (png, times) = video::contact_sheet(path, dur, *tiles, 480)?;
            std::fs::write(out, png)?;
            eprintln!("tiles at seconds: {}", times.iter().map(|t| format!("{t:.1}")).collect::<Vec<_>>().join(", "));
            println!("{}", out.display());
            Ok(())
        }
        RecordCmd::Frames { path, at, every, scene, max, out_dir } => {
            let dur = video::duration(path)?;
            let times: Vec<f32> = if let Some(at) = at {
                at.clone()
            } else if let Some(th) = scene {
                video::scene_changes(path, *th, 0.0, dur)?
            } else {
                let step = every.unwrap_or((dur / (*max as f32 - 1.0).max(1.0)).max(0.1));
                let mut v = Vec::new();
                let mut t = 0.0;
                while t <= dur + 1e-3 {
                    v.push(t);
                    t += step;
                }
                v
            };
            let times = video::thin(times, *max);
            std::fs::create_dir_all(out_dir)?;
            for (t, png) in video::frames_at(path, &times, 800)? {
                let f = out_dir.join(format!("frame-{t:07.2}.png"));
                std::fs::write(&f, png)?;
                println!("{}", f.display());
            }
            Ok(())
        }
    }
}

fn print_json(v: &serde_json::Value) -> Result<()> {
    println!("{}", serde_json::to_string_pretty(v)?);
    Ok(())
}

/// Type ASCII through the virtual keyboard; anything else through the
/// clipboard and Ctrl+V, which needs the session's Wayland or X socket.
pub fn type_text(c: &Client, text: &str) -> Result<()> {
    if text.is_ascii() {
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

fn paste(c: &Client, text: &str) -> Result<()> {
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
