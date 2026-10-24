//! xcua: kmscua's MCP tools over an X server it owns (or attaches to).
//!
//! `xcua up` makes sure a display answers (starting an Xvfb if needed) and
//! writes `DISPLAY=:N` to the state dir's `env` file for the agent's other
//! processes; `xcua mcp` does the same and then serves the tools on stdio.
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
#[command(name = "xcua", version, about = "kmscua's computer-use tools over X11 (Xvfb), in one unprivileged process")]
struct Cli {
    /// Where the env file, pidfile and recordings live
    /// [default: $XDG_STATE_HOME/xcua or ~/.local/state/xcua].
    #[arg(long, global = true, env = "XCUA_STATE_DIR")]
    state_dir: Option<PathBuf>,
    /// X display to use (":N"); default: xcua's own Xvfb, then $DISPLAY.
    #[arg(long, global = true)]
    display: Option<String>,
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Make sure a display answers: attach to one, or start an Xvfb that
    /// stays until `xcua down`. Idempotent.
    Up {
        #[arg(long, default_value = "1920x1080")]
        size: String,
    },
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
        /// Screen size if an Xvfb has to be started.
        #[arg(long, default_value = "1920x1080")]
        size: String,
        /// Capture through MIT-SHM (memfd), falling back to GetImage.
        #[arg(long, env = "XCUA_SHM")]
        shm: bool,
    },
    /// Write one screenshot to a file (PNG, or JPEG for .jpg/--jpeg).
    Screenshot {
        #[arg(short, long)]
        output: PathBuf,
        #[arg(long)]
        max_side: Option<u32>,
        #[arg(long)]
        jpeg: bool,
        #[arg(long)]
        no_cursor: bool,
        #[arg(long, env = "XCUA_SHM")]
        shm: bool,
    },
    /// Display, extensions, capture path, focus, recording dir.
    Doctor,
}

fn main() -> Result<()> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("warn"))
        .target(env_logger::Target::Stderr)
        .init();
    let cli = Cli::parse();
    let state = State::new(cli.state_dir.clone())?;
    let display = cli.display.as_deref();
    match &cli.cmd {
        Cmd::Up { size } => {
            let up = xvfb::up(&state, display, xvfb::parse_size(size)?, Owner::Up)?;
            match &up {
                Up::Attached { display } => println!("attached to {display}"),
                Up::Reused { display, pid } => println!("already up: {display} (Xvfb pid {pid})"),
                Up::Started { display, pid } => println!("started Xvfb on {display} (pid {pid})"),
            }
            println!("DISPLAY={}  (also in {})", up.display(), state.env_path().display());
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
        Cmd::Mcp { max_side, tools, size, shm } => {
            let core_only = match tools.as_str() {
                "all" => false,
                "core" => true,
                other => bail!("--tools must be all or core, not {other}"),
            };
            mcp(&state, display, xvfb::parse_size(size)?, *shm, *max_side, core_only)
        }
        Cmd::Screenshot { output, max_side, jpeg, no_cursor, shm } => {
            let d = xvfb::resolve(&state, display)?;
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
        Cmd::Doctor => doctor(&state, display),
    }
}

fn mcp(state: &State, display: Option<&str>, size: (u32, u32), shm: bool, max_side: Option<u32>, core_only: bool) -> Result<()> {
    let me = Owner::Mcp(std::process::id());
    // Before the runtime: the Xvfb must not inherit tokio's threads' state.
    let up = xvfb::up(state, display, size, me)?;
    let d = up.display().to_string();
    log::info!("xcua mcp on {d} ({up:?})");
    let run = || -> Result<()> {
        let backend = Arc::new(XBackend::open(&d, shm, &state.recordings())?);
        let opts = ServeOptions {
            max_side,
            core_only,
            name: Some("xcua".into()),
            instructions: Some(instructions(&d, state)),
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
        rt.shutdown_timeout(Duration::from_secs(2));
        backend.release_all();
        served
    };
    let r = run();
    xvfb::release(state, me);
    r
}

fn instructions(display: &str, state: &State) -> String {
    format!(
        "Computer use on the X display {display}, an Xvfb or an X server this tool attached to. Take a screenshot first; every coordinate you pass is in that image's pixels, and zoom never changes that. Actions answer OK and never leave a key or button held (except left_mouse_down). There is no window manager unless you start one: clicking a window gives it the keyboard. Start GUI apps with DISPLAY={display} (or source {}). type takes any Unicode text. Use wait_for_stable after actions that trigger loading, or pass settle=true on the action.",
        state.env_path().display()
    )
}

fn doctor(state: &State, display: Option<&str>) -> Result<()> {
    print!("{}", xvfb::status(state));
    let d = match xvfb::resolve(state, display) {
        Ok(d) => d,
        Err(e) => {
            println!("display: unavailable: {e:#}");
            return Ok(());
        }
    };
    let b = XBackend::open(&d, std::env::var_os("XCUA_SHM").is_some(), &state.recordings())?;
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
