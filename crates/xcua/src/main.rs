//! xcua: kmscua's MCP tools over an X server it owns (or attaches to).
//!
//! `xcua up` makes sure a display answers (starting an Xvfb if needed) and
//! writes `DISPLAY=:N` to the state dir's `env` file for the agent's other
//! processes; `xcua mcp`, `screenshot` and `doctor` do the same first. The
//! shell's `$DISPLAY` is never used unless `--display env` says so: the
//! agent must not land on the user's screen by accident. `--display` on any
//! command but `up` is for that run only and leaves the state dir alone.
//! The ownership rule for the Xvfb is in `xvfb.rs`.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use clap::{Parser, Subcommand};
use kmscua_mcp::ServeOptions;
use kmscua_proto::ImageFormat;
use xcua::xvfb::{self, Owner, State, Up};
use xcua::{pixels, x11, XBackend};

#[derive(Parser)]
#[command(name = "xcua", version = env!("KMSCUA_BUILD"), about = "kmscua's computer-use tools over X11 (Xvfb), in one unprivileged process")]
struct Cli {
    /// Where the env file, pidfile and recordings live
    /// [default: $XDG_STATE_HOME/xcua or ~/.local/state/xcua].
    #[arg(long, global = true, env = "XCUA_STATE_DIR")]
    state_dir: Option<PathBuf>,
    /// X display to use: ":N", or "env" for the shell's $DISPLAY. Without
    /// it xcua uses its own Xvfb (started if needed) and never $DISPLAY.
    /// Only `up` records it in the state dir; other commands use it for the
    /// one run.
    #[arg(long, global = true)]
    display: Option<String>,
    /// Screen size if an Xvfb has to be started.
    #[arg(long, global = true, default_value = "1920x1080")]
    size: String,
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Make sure a display answers: start (or adopt) xcua's Xvfb, which
    /// stays until `xcua down`, or record --display as the one to use.
    /// Idempotent.
    Up,
    /// Stop the Xvfb xcua started and forget the display.
    Down,
    /// Display, Xvfb pid and owner, env file.
    Status,
    /// Serve the MCP tools on stdio. Starts an Xvfb when none answers; one
    /// started here stops when this process exits (run `xcua up` first to
    /// keep it).
    Mcp {
        /// Longest side of screenshots sent to the model.
        #[arg(long)]
        max_side: Option<u32>,
        /// `all`, or `core` for only the vendor-shaped tools.
        #[arg(long, default_value = "all")]
        tools: String,
        /// Capture through MIT-SHM (memfd), falling back to GetImage.
        /// XCUA_SHM=1 turns it on, 0/false/no/off or empty leave it off.
        #[arg(long, env = "XCUA_SHM", value_parser = clap::builder::FalseyValueParser::new())]
        shm: bool,
    },
    /// Write one screenshot to a file (PNG, or JPEG for .jpg/--jpeg). Starts
    /// an Xvfb like `up` when none is running.
    Screenshot {
        #[arg(short, long)]
        output: PathBuf,
        #[arg(long)]
        max_side: Option<u32>,
        #[arg(long)]
        jpeg: bool,
        #[arg(long)]
        no_cursor: bool,
        #[arg(long, env = "XCUA_SHM", value_parser = clap::builder::FalseyValueParser::new())]
        shm: bool,
    },
    /// Display, extensions, capture path, focus, recording dir. Starts an
    /// Xvfb like `up` when none is running.
    Doctor,
}

fn main() -> Result<()> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("warn"))
        .target(env_logger::Target::Stderr)
        .init();
    let cli = Cli::parse();
    let state = State::new(cli.state_dir.clone())?;
    let display = xvfb::explicit(cli.display.as_deref())?;
    let display = display.as_deref();
    let size = xvfb::parse_size(&cli.size)?;
    match &cli.cmd {
        Cmd::Up => {
            let up = xvfb::up(&state, display, size, Owner::Up)?;
            println!("{}", describe_up(&up));
            println!("export DISPLAY={}  (also in {})", up.display(), state.env_path().display());
            Ok(())
        }
        Cmd::Down => {
            println!("{}", xvfb::down(&state)?);
            Ok(())
        }
        Cmd::Status => {
            print!("{}", xvfb::status(&state));
            Ok(())
        }
        Cmd::Mcp { max_side, tools, shm } => {
            let core_only = match tools.as_str() {
                "all" => false,
                "core" => true,
                other => bail!("--tools must be all or core, not {other}"),
            };
            mcp(&state, display, size, *shm, *max_side, core_only)
        }
        Cmd::Screenshot { output, max_side, jpeg, no_cursor, shm } => {
            let d = ensure(&state, display, size)?;
            let mut cap = x11::Capture::open(&d, *shm)?;
            let shot = cap.grab(!no_cursor, None)?;
            let ext_jpeg = matches!(output.extension().and_then(|e| e.to_str()), Some("jpg" | "jpeg"));
            let format = if *jpeg || ext_jpeg { ImageFormat::Jpeg } else { ImageFormat::Png };
            let (info, bytes) = pixels::finish(shot, *max_side, format, None)?;
            std::fs::write(output, &bytes).with_context(|| format!("write {}", output.display()))?;
            println!(
                "{}: {}x{} from {}x{} on {d}, grab {:.1} ms, encode {:.1} ms{}",
                output.display(),
                info.width,
                info.height,
                info.source_width,
                info.source_height,
                info.grab_ms,
                info.encode_ms,
                if cap.shm_active() { ", MIT-SHM" } else { "" }
            );
            Ok(())
        }
        Cmd::Doctor => doctor(&state, display, size),
    }
}

fn describe_up(up: &Up) -> String {
    match up {
        Up::Attached { display } => format!("attached to {display}"),
        Up::Reused { display, pid } => format!("already up: {display} (Xvfb pid {pid})"),
        Up::Started { display, pid } => format!("started Xvfb on {display} (pid {pid}); it stays until `xcua down`"),
    }
}

/// The display for a one-shot command. `--display` is used as given and
/// the state dir is not touched; otherwise `up`, with a line on stderr when
/// an Xvfb had to be started (it stays, as after `xcua up`).
fn ensure(state: &State, display: Option<&str>, size: (u32, u32)) -> Result<String> {
    if let Some(d) = display {
        if !xvfb::answers(d) {
            bail!("no X server answers on {d}");
        }
        return Ok(d.to_string());
    }
    let up = xvfb::up(state, None, size, Owner::Up)?;
    if matches!(up, Up::Started { .. }) {
        eprintln!("{}", describe_up(&up));
    }
    Ok(up.display().to_string())
}

fn mcp(state: &State, display: Option<&str>, size: (u32, u32), shm: bool, max_side: Option<u32>, core_only: bool) -> Result<()> {
    let me = std::process::id();
    // Before the runtime: the Xvfb must not inherit tokio's threads' state.
    // With --display the state dir is not touched and nothing is released.
    let d = match display {
        Some(d) => ensure(state, Some(d), size)?,
        None => {
            let up = xvfb::up(state, None, size, Owner::Mcp(me))?;
            log::info!("xcua mcp: {up:?}");
            up.display().to_string()
        }
    };
    let run = || -> Result<()> {
        let mut backend = XBackend::open(&d, shm, &state.recordings())?;
        if display.is_none() {
            // Our Xvfb went away (another mcp stopped it, it crashed): find
            // or start one again, as a user of it.
            let st = state.clone();
            backend = backend.with_reup(Box::new(move || Ok(xvfb::up(&st, None, size, Owner::Mcp(me))?.display().to_string())));
        }
        let backend = Arc::new(backend);
        let opts = ServeOptions {
            max_side,
            core_only,
            name: Some("xcua".into()),
            instructions: Some(instructions(&d, state)),
            descriptions: DESCRIPTIONS.iter().map(|(n, t)| (n.to_string(), t.to_string())).collect(),
        };
        let rt = tokio::runtime::Runtime::new()?;
        let served = rt.block_on(async {
            use tokio::signal::unix::{signal, SignalKind};
            let mut term = signal(SignalKind::terminate())?;
            let mut hup = signal(SignalKind::hangup())?;
            let mut int = signal(SignalKind::interrupt())?;
            tokio::select! {
                r = kmscua_mcp::serve_stdio(backend.clone(), opts) => r,
                _ = term.recv() => Ok(()),
                _ = hup.recv() => Ok(()),
                _ = int.recv() => Ok(()),
            }
        });
        // Cut a running hold_key short, give it a moment to let go of the
        // input, release everything; never wait out a 100 s hold.
        backend.shutdown(Duration::from_secs(2));
        rt.shutdown_timeout(Duration::from_secs(1));
        served
    };
    let r = run();
    if display.is_none() {
        xvfb::release(state, me);
    }
    r
}

/// cua's tool descriptions that would be wrong here: no GPU scanout, no
/// clipboard, no MP4, no accessibility bus.
const DESCRIPTIONS: &[(&str, &str)] = &[
    (
        "screenshot",
        "Take a screenshot of the whole X display, pointer included. The returned image is what subsequent click coordinates are relative to.",
    ),
    (
        "type",
        "Type a string of text into the focused window. Any Unicode text works: characters outside the keyboard layout go through a briefly rebound keycode, no clipboard involved.",
    ),
    (
        "record_start",
        "Start recording the screen as JPEG stills: a still each time the picture changes (5 fps by default), stopped at max_seconds (600), a 200 MB budget or low disk space. Every tool call you make while recording is stamped into a timeline. Stop with record_stop; it returns a manifest and a contact sheet, never the stills themselves.",
    ),
    (
        "record_stop",
        "Stop the recording. Returns the stills directory, duration, the numbered timeline of every action taken while recording (for record_frames `between`), and a contact sheet of evenly spaced stills. If the recording stopped early (budget, disk), the reason is in the result.",
    ),
    (
        "get_focused",
        "Which X window has the keyboard (or is active, when a window manager says so): its id, title, class, pid and geometry, read from window properties. It does not read text inside the window; use zoom for that. Nothing here changes the screen.",
    ),
    (
        "switch_display",
        "Restrict screenshots to one monitor (by name as doctor lists them under displays, or \"primary\"), or pass \"all\" to see the whole screen again. Returns a screenshot of the selection; coordinates you pass afterwards are pixels of that screenshot. An Xvfb has one screen, so this matters only on an X server with several RandR monitors.",
    ),
    ("doctor", "Report the X display, its extensions, the capture path, keyboard focus and the recording directory, with what to do about anything missing."),
];

/// An on/off environment variable: unset, empty, 0, false, no, off are off.
fn env_flag(name: &str) -> bool {
    std::env::var(name)
        .map(|v| !matches!(v.trim().to_ascii_lowercase().as_str(), "" | "0" | "false" | "no" | "off"))
        .unwrap_or(false)
}

fn instructions(display: &str, state: &State) -> String {
    format!(
        "Computer use on the X display {display}, an Xvfb or an X server this tool attached to. Take a screenshot first; every coordinate you pass is in that image's pixels, and zoom never changes that. Actions answer OK and never leave a key or button held (except left_mouse_down). There is no window manager unless you start one: clicking a window gives it the keyboard. Start GUI apps with DISPLAY={display} (or source {}). type takes any Unicode text. Use wait_for_stable after actions that trigger loading, or pass settle=true on the action.",
        state.env_path().display()
    )
}

fn doctor(state: &State, display: Option<&str>, size: (u32, u32)) -> Result<()> {
    let d = match ensure(state, display, size) {
        Ok(d) => d,
        Err(e) => {
            print!("{}", xvfb::status(state));
            println!("display: unavailable: {e:#}");
            return Ok(());
        }
    };
    print!("{}", xvfb::status(state));
    let b = XBackend::open(&d, env_flag("XCUA_SHM"), &state.recordings())?;
    let r = b.doctor_report();
    let s = |k: &str| r[k].as_str().unwrap_or("?").to_string();
    println!("using display: {} ({})", s("display"), s("size"));
    if let Some(ext) = r["extensions"].as_object() {
        let list: Vec<String> = ext
            .iter()
            .map(|(k, v)| format!("{k} {}", if v.as_bool() == Some(true) { "yes" } else { "NO" }))
            .collect();
        println!("extensions: {}", list.join(", "));
    }
    println!("capture: {}", s("capture"));
    println!(
        "window manager: {}",
        if r["window_manager"].as_bool() == Some(true) { "yes (it handles focus)" } else { "none (xcua focuses the clicked window)" }
    );
    println!("spare keycodes: {}", r["spare_keycodes"]);
    println!("recording dir: {} (codec {})", s("record_dir"), xcua::backend::RECORD_CODEC);
    println!("focus:");
    for line in s("focus").lines() {
        println!("  {line}");
    }
    if let Some(notes) = r["notes"].as_array() {
        for n in notes {
            println!("note: {}", n.as_str().unwrap_or_default());
        }
    }
    Ok(())
}
