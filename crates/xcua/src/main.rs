//! xcua: kmscua's MCP tools over an X server it owns (or attaches to).
//!
//! `xcua up` makes sure a display answers (starting an Xvfb if needed) and
//! writes `export DISPLAY=:N` to the state dir's `env` file for the agent's
//! other processes; `xcua mcp`, `screenshot`, `doctor` and every verb do the
//! same first. The shell's `$DISPLAY` is never used unless `--display env`
//! says so: the agent must not land on the user's screen by accident.
//! `--display` on any command but `up` is for that run only and leaves the
//! state dir alone. The ownership rule for the Xvfb is in `xvfb.rs`.
//!
//! The verbs (`click`, `type`, `key`, … `record`) are the MCP tools as
//! one-shot commands, for agents that run shell commands but cannot
//! register an MCP server. Their coordinates are pixels of the LAST
//! `xcua screenshot`, mapped through `<state>/view.json` (see `view.rs`).

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{bail, Result};
use clap::{Parser, Subcommand};
use kmscua_mcp::{video, ServeOptions};
use kmscua_proto::Button;
use serde_json::json;
use xcua::cli::{self, Ctx, Out};
use xcua::recd;
use xcua::view::parse_xywh;
use xcua::xvfb::{self, Owner, State, Up};
use xcua::XBackend;

const COORDS: &str = "Coordinates are pixels of the LAST `xcua screenshot` image (mapped through <state>/view.json, as the MCP tools map them): after `screenshot --max-side 1280`, `click 640 400` is the middle of that 1280-wide image. Take a new screenshot whenever the screen may have changed.";

#[derive(Parser)]
#[command(
    name = "xcua",
    version = env!("KMSCUA_BUILD"),
    about = "kmscua's computer-use tools over X11 (Xvfb), in one unprivileged process",
    after_help = COORDS
)]
struct Cli {
    /// Where the env file, pidfile, view and recordings live
    /// [default: $XDG_STATE_HOME/xcua or ~/.local/state/xcua].
    #[arg(long, global = true, env = "XCUA_STATE_DIR")]
    state_dir: Option<PathBuf>,
    /// X display to use: ":N", or "env" for the shell's $DISPLAY. Without
    /// it xcua uses its own Xvfb (started if needed) and never $DISPLAY.
    /// Only `up` records it in the state dir; other commands use it for the
    /// one run.
    #[arg(long, global = true)]
    display: Option<String>,
    /// Screen size WxH if an Xvfb has to be started [default: 1920x1080].
    /// `up` refuses when xcua's display already runs at another size.
    #[arg(long, global = true)]
    size: Option<String>,
    /// Print the result as JSON (errors too, as {"ok": false, "error": …}).
    #[arg(long, global = true)]
    json: bool,
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Make sure a display answers: start (or adopt) xcua's Xvfb, which
    /// stays until `xcua down`, or record --display as the one to use.
    /// Idempotent.
    Up {
        /// Stop xcua's running Xvfb (and its window manager) and start a
        /// new one, at --size. Apps on the old one close.
        #[arg(long)]
        restart: bool,
        /// Window manager: `auto` (the first installed of jwm, openbox,
        /// fluxbox, icewm, twm), `none` (stop it), or a command. Default:
        /// leave it as it is (none on a new display). Browsers want one.
        #[arg(long)]
        wm: Option<String>,
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
        /// Capture through MIT-SHM (memfd), falling back to GetImage.
        /// XCUA_SHM=1 turns it on, 0/false/no/off or empty leave it off.
        #[arg(long, env = "XCUA_SHM", value_parser = clap::builder::FalseyValueParser::new())]
        shm: bool,
    },
    /// Take a screenshot (PNG, or JPEG for .jpg/--jpeg). Later coordinates
    /// are pixels of THIS image.
    #[command(after_help = COORDS)]
    Screenshot {
        #[arg(short, long, default_value = "screenshot.png")]
        output: PathBuf,
        /// Longest side of the image; coordinates scale with it.
        #[arg(long)]
        max_side: Option<u32>,
        #[arg(long)]
        jpeg: bool,
        #[arg(long)]
        no_cursor: bool,
        #[arg(long, env = "XCUA_SHM", value_parser = clap::builder::FalseyValueParser::new())]
        shm: bool,
    },
    /// Display, extensions, capture path, focus, window manager, recording
    /// dir. Never starts a display.
    Doctor,
    /// Click at X Y (last screenshot's pixels). The clicked window gets the
    /// keyboard.
    #[command(after_help = COORDS)]
    Click {
        x: i32,
        y: i32,
        #[arg(long, default_value = "left", value_parser = parse_button)]
        button: Button,
        #[arg(long, conflicts_with = "triple")]
        double: bool,
        #[arg(long)]
        triple: bool,
        /// Modifier keys held during the click, e.g. ctrl or ctrl+shift.
        #[arg(long)]
        mods: Option<String>,
    },
    /// Move the pointer to X Y (last screenshot's pixels).
    #[command(name = "move", after_help = COORDS)]
    Move { x: i32, y: i32 },
    /// Drag with the left button from X1 Y1 to X2 Y2.
    #[command(after_help = COORDS)]
    Drag {
        x1: i32,
        y1: i32,
        x2: i32,
        y2: i32,
        #[arg(long)]
        mods: Option<String>,
    },
    /// Wheel detents at X Y: --dy 3 scrolls down, --dy -3 up, --dx right/left.
    #[command(after_help = COORDS, allow_negative_numbers = true)]
    Scroll {
        x: i32,
        y: i32,
        #[arg(long, default_value_t = 0, allow_negative_numbers = true)]
        dy: i32,
        #[arg(long, default_value_t = 0, allow_negative_numbers = true)]
        dx: i32,
        #[arg(long)]
        mods: Option<String>,
    },
    /// Type text into the focused window; any Unicode. `-` reads stdin
    /// (one trailing newline dropped). Text may start with `-`; `--` ends
    /// the options if it looks like one.
    Type {
        #[arg(allow_hyphen_values = true)]
        text: String,
        /// Milliseconds between key events; raise it for apps that drop
        /// fast input.
        #[arg(long, default_value_t = 12)]
        delay_ms: u32,
    },
    /// Press key chords in order, xdotool names: `key ctrl+l`, `key Return`,
    /// `key ctrl+a BackSpace`.
    Key {
        #[arg(required = true)]
        combos: Vec<String>,
        #[arg(long, default_value_t = 1)]
        repeat: u32,
    },
    /// Hold a chord down for --seconds, then release it.
    Hold {
        keys: String,
        #[arg(long)]
        seconds: f32,
    },
    /// Where the pointer is, in the last screenshot's pixels.
    Cursor,
    /// Magnify X0 Y0 X1 Y1 of the last screenshot into -o at up to native
    /// resolution. Coordinates keep referring to the last screenshot.
    #[command(after_help = COORDS)]
    Zoom {
        x0: i32,
        y0: i32,
        x1: i32,
        y1: i32,
        #[arg(short, long, default_value = "zoom.png")]
        output: PathBuf,
        #[arg(long)]
        max_side: Option<u32>,
    },
    /// Wait until the screen (or --region x,y,w,h) stops changing. Exit 3
    /// if it is still changing at --timeout.
    WaitStable {
        /// Milliseconds.
        #[arg(long, default_value_t = 8000)]
        timeout: u32,
        /// Milliseconds the screen must stay still.
        #[arg(long, default_value_t = 600)]
        quiet: u32,
        #[arg(long, default_value_t = 0.002)]
        threshold: f32,
        /// x,y,w,h in the last screenshot's pixels.
        #[arg(long)]
        region: Option<String>,
    },
    /// Wait until the screen (or --region x,y,w,h) differs from now. Exit 3
    /// if nothing changed by --timeout.
    WaitChange {
        #[arg(long, default_value_t = 8000)]
        timeout: u32,
        #[arg(long, default_value_t = 0.01)]
        threshold: f32,
        #[arg(long)]
        region: Option<String>,
    },
    /// Which window has the keyboard: id, title, class, pid, geometry.
    Focused,
    /// Wait until an app window (title/class substring) is mapped and, with
    /// --painted, drawn into (its screen area not one colour). Exit 3 on
    /// timeout. A browser's first window can take 30 s or more.
    WaitWindow {
        #[arg(long)]
        name: Option<String>,
        #[arg(long)]
        class: Option<String>,
        #[arg(long)]
        painted: bool,
        /// Milliseconds.
        #[arg(long, default_value_t = 30000)]
        timeout: u32,
    },
    /// Make a window fill the screen: the one matching --class, else the
    /// focused one. Uses the window manager when there is one.
    Maximize {
        #[arg(long)]
        class: Option<String>,
    },
    /// Run several actions (the MCP computer_batch shape, JSON or `-` for
    /// stdin); stops at the first failure. Screenshots go to --out-dir.
    Batch {
        /// `{"actions": [...]}` or `[...]`, or `-` for stdin.
        #[arg(value_name = "JSON")]
        actions: String,
        #[arg(long)]
        out_dir: Option<PathBuf>,
    },
    /// Record the screen as JPEG stills, across invocations.
    #[command(subcommand)]
    Record(RecordCmd),
}

#[derive(Subcommand)]
enum RecordCmd {
    /// Start a detached recorder; returns once it records.
    Start {
        #[arg(long)]
        name: Option<String>,
        #[arg(long)]
        fps: Option<u32>,
        #[arg(long)]
        max_side: Option<u32>,
        #[arg(long)]
        max_seconds: Option<u32>,
        /// Run the recorder in this process until SIGTERM/SIGINT (what the
        /// detached recorder runs).
        #[arg(long)]
        foreground: bool,
    },
    /// Whether a recording runs, and how far it is.
    Status,
    /// Stamp a note into the recording's timeline.
    Mark {
        #[arg(allow_hyphen_values = true)]
        note: String,
    },
    /// Stop the recording: manifest, timeline, and a contact sheet.
    Stop {
        /// Where to write the contact sheet [default: beside the stills].
        #[arg(long)]
        sheet: Option<PathBuf>,
        #[arg(long)]
        no_sheet: bool,
        #[arg(long, default_value_t = 6)]
        tiles: u32,
    },
    /// Evenly spaced stills from the last recording (or --path) into -o.
    Frames {
        #[arg(long, default_value_t = 6)]
        count: u32,
        #[arg(short, long)]
        output: PathBuf,
        #[arg(long)]
        path: Option<PathBuf>,
        #[arg(long, default_value_t = 1280)]
        max_side: u32,
    },
}

fn parse_button(s: &str) -> Result<Button, String> {
    match s.to_ascii_lowercase().as_str() {
        "left" => Ok(Button::Left),
        "right" => Ok(Button::Right),
        "middle" => Ok(Button::Middle),
        _ => Err(format!("unknown button {s}: want left, right or middle")),
    }
}

fn mods(m: &Option<String>) -> Vec<String> {
    m.iter().map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect()
}

fn main() {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("warn"))
        .target(env_logger::Target::Stderr)
        .init();
    let cli = match Cli::try_parse() {
        Ok(c) => c,
        Err(e) => {
            // Usage errors honour --json too; help and --version do not.
            let wants_json = std::env::args().any(|a| a == "--json");
            if wants_json && e.use_stderr() {
                println!("{}", json!({ "ok": false, "error": e.to_string().trim() }));
                std::process::exit(2);
            }
            e.exit();
        }
    };
    let json = cli.json;
    let code = match run(cli) {
        Ok(out) => {
            if json {
                println!("{}", serde_json::to_string_pretty(&out.json).unwrap_or_default());
            } else if !out.text.is_empty() {
                println!("{}", out.text);
            }
            out.code
        }
        Err(e) => {
            if json {
                println!("{}", json!({ "ok": false, "error": format!("{e:#}") }));
            } else {
                eprintln!("xcua: error: {e:#}");
            }
            1
        }
    };
    std::process::exit(code);
}

fn run(cli: Cli) -> Result<Out> {
    let state = State::new(cli.state_dir.clone())?;
    let display = xvfb::explicit(cli.display.as_deref())?;
    let display = display.as_deref();
    let size_opt = cli.size.as_deref().map(xvfb::parse_size).transpose()?;
    let size = size_opt.unwrap_or(xvfb::DEFAULT_SIZE);
    let ensure = |st: &State, d: Option<&str>, size: (u32, u32)| ensure(st, d, size, size_opt.is_some());
    let open = |shm: bool| -> Result<Ctx> { Ctx::open(state.clone(), &ensure(&state, display, size)?, shm, true) };
    // Input verbs take turns across shells (see cli::input_lock).
    let _input = match &cli.cmd {
        Cmd::Click { .. }
        | Cmd::Move { .. }
        | Cmd::Drag { .. }
        | Cmd::Scroll { .. }
        | Cmd::Type { .. }
        | Cmd::Key { .. }
        | Cmd::Hold { .. }
        | Cmd::Batch { .. } => Some(cli::input_lock(&state)?),
        _ => None,
    };
    let region = |r: &Option<String>| r.as_deref().map(parse_xywh).transpose();
    match &cli.cmd {
        Cmd::Up { restart, wm } => {
            let opts = xvfb::UpOpts { size: size_opt, restart: *restart, wm: wm.clone() };
            let (up, notes) = xvfb::up_with(&state, display, Owner::Up, size, &opts)?;
            let (kind, pid) = match &up {
                Up::Attached { .. } => ("attached", None),
                Up::Reused { pid, .. } => ("reused", Some(*pid)),
                Up::Started { pid, .. } => ("started", Some(*pid)),
            };
            let screen = xvfb::display_size(up.display()).map(|(w, h)| format!("{w}x{h}"));
            let mut text = format!("{}{}", describe_up(&up), screen.as_deref().map(|s| format!(", {s}")).unwrap_or_default());
            for n in &notes {
                text.push_str(&format!("\n{n}"));
            }
            text.push_str(&format!("\nexport DISPLAY={}  (also in {})", up.display(), state.env_path().display()));
            Ok(Out::ok(
                text,
                json!({ "ok": true, "result": kind, "display": up.display(), "size": screen, "pid": pid, "env_file": state.env_path(), "notes": notes, "wm": state.wm().map(|(c, p, alive)| json!({ "cmd": c, "pid": p.pid, "alive": alive })) }),
            ))
        }
        Cmd::Down => {
            let msg = xvfb::down(&state)?;
            Ok(Out::ok(msg.clone(), json!({ "ok": true, "message": msg })))
        }
        Cmd::Status => {
            let text = xvfb::status(&state);
            let rec = recd::read(&state).map(|r| r.info);
            Ok(Out::ok(
                text.trim_end(),
                json!({ "state_dir": state.dir, "display": state.display(), "pid": state.pid().map(|p| p.pid), "text": text, "recording": rec }),
            ))
        }
        Cmd::Mcp { max_side, tools, shm } => {
            let core_only = match tools.as_str() {
                "all" => false,
                "core" => true,
                other => bail!("--tools must be all or core, not {other}"),
            };
            mcp(&state, display, size, *shm, *max_side, core_only)?;
            Ok(Out::ok("", json!({ "ok": true })))
        }
        Cmd::Screenshot { output, max_side, jpeg, no_cursor, shm } => {
            Ctx::open(state.clone(), &ensure(&state, display, size)?, *shm, false)?.screenshot(output, *max_side, *jpeg, !no_cursor)
        }
        Cmd::Doctor => doctor(&state, display, size),
        Cmd::Click { x, y, button, double, triple, mods: m } => {
            let count = if *triple { 3 } else if *double { 2 } else { 1 };
            open(false)?.click(*x, *y, *button, count, &mods(m))
        }
        Cmd::Move { x, y } => open(false)?.move_to(*x, *y),
        Cmd::Drag { x1, y1, x2, y2, mods: m } => open(false)?.drag(Some((*x1, *y1)), (*x2, *y2), &mods(m)),
        Cmd::Scroll { x, y, dy, dx, mods: m } => {
            if *dx == 0 && *dy == 0 {
                bail!("scroll needs --dy and/or --dx (detents; --dy 3 scrolls down)");
            }
            open(false)?.scroll(Some((*x, *y)), *dx, *dy, &mods(m))
        }
        Cmd::Type { text, delay_ms } => open(false)?.type_slowly(&cli::text_arg(text)?, *delay_ms),
        Cmd::WaitWindow { name, class, painted, timeout } => open(false)?.wait_window(name.as_deref(), class.as_deref(), *painted, *timeout),
        Cmd::Maximize { class } => open(false)?.maximize(class.as_deref()),
        Cmd::Key { combos, repeat } => open(false)?.key(combos, *repeat),
        Cmd::Hold { keys, seconds } => open(false)?.hold(keys, *seconds),
        Cmd::Cursor => open(false)?.cursor(),
        Cmd::Zoom { x0, y0, x1, y1, output, max_side } => open(false)?.zoom([*x0, *y0, *x1, *y1], output, *max_side),
        Cmd::WaitStable { timeout, quiet, threshold, region: r } => open(false)?.wait_stable(region(r)?, *quiet, *timeout, *threshold),
        Cmd::WaitChange { timeout, threshold, region: r } => open(false)?.wait_change(region(r)?, *timeout, *threshold),
        Cmd::Focused => open(false)?.focused(),
        Cmd::Batch { actions: text, out_dir } => {
            let actions = cli::parse_batch(&cli::text_arg(text)?)?;
            let dir = out_dir.clone().unwrap_or_else(|| cli::shots_dir(&state));
            open(false)?.batch(&actions, &dir)
        }
        Cmd::Record(rc) => record(&state, display, size, rc),
    }
}

fn record(state: &State, display: Option<&str>, size: (u32, u32), rc: &RecordCmd) -> Result<Out> {
    match rc {
        RecordCmd::Start { name, fps, max_side, max_seconds, foreground } => {
            let opts = recd::StartOpts { name: name.clone(), fps: *fps, max_side: *max_side, max_seconds: *max_seconds };
            let d = ensure(state, display, size, false)?;
            let s = if *foreground { recd::foreground(state, &d, &opts)? } else { recd::start(state, &d, &opts)? };
            let i = &s.info;
            Ok(Out::ok(
                format!(
                    "{} {} at {}x{} {} fps with {}",
                    if i.recording { "recording" } else { "recorded" },
                    i.path.as_deref().unwrap_or("?"),
                    i.width,
                    i.height,
                    i.fps,
                    i.codec.as_deref().unwrap_or("?")
                ),
                serde_json::to_value(&s)?,
            ))
        }
        RecordCmd::Status => {
            let live = recd::running(state).is_some();
            match recd::read(state) {
                Some(s) => Ok(Out::ok(
                    format!(
                        "{} {} | {:.1}s, {} stills, {} dropped{}",
                        if live && s.info.recording { "recording" } else { "not recording; last" },
                        s.info.path.as_deref().unwrap_or("?"),
                        s.info.seconds,
                        s.info.frames,
                        s.info.dropped,
                        s.info.error.as_deref().map(|e| format!(" | {e}")).unwrap_or_default()
                    ),
                    json!({ "recording": live && s.info.recording, "state": s }),
                )),
                None => Ok(Out::ok("not recording", json!({ "recording": false }))),
            }
        }
        RecordCmd::Mark { note } => {
            recd::mark(state, note)?;
            Ok(Out::ok("OK", json!({ "ok": true })))
        }
        RecordCmd::Stop { sheet, no_sheet, tiles } => {
            let s = recd::stop(state)?;
            let i = &s.info;
            let path = PathBuf::from(i.path.clone().unwrap_or_default());
            let mut lines = vec![format!(
                "recording {} | {:.1}s, {} stills at {} fps, {}x{}, {} | finalized={}{} | manifest {}",
                path.display(),
                i.seconds,
                i.frames,
                i.fps,
                i.width,
                i.height,
                i.codec.as_deref().unwrap_or("?"),
                i.finalized.unwrap_or(false),
                i.error.as_deref().map(|e| format!(" | {e}")).unwrap_or_default(),
                s.manifest.as_deref().unwrap_or("?")
            )];
            if !s.timeline.is_empty() {
                lines.push("timeline:".into());
                for (n, (t, what)) in s.timeline.iter().enumerate() {
                    lines.push(format!("  [{n}] {t:7.2}s  {what}"));
                }
            }
            let mut sheet_path = None;
            if !no_sheet {
                let out = sheet.clone().unwrap_or_else(|| path.with_extension("sheet.png"));
                match video::contact_sheet(&path, i.seconds.max(0.1), (*tiles).clamp(1, 16), 480) {
                    Ok((png, times)) => {
                        std::fs::write(&out, png)?;
                        lines.push(format!(
                            "contact sheet {} (tiles left to right, top to bottom, at seconds: {})",
                            out.display(),
                            times.iter().map(|t| format!("{t:.1}")).collect::<Vec<_>>().join(", ")
                        ));
                        sheet_path = Some(out);
                    }
                    Err(e) => lines.push(format!("contact sheet unavailable: {e:#}")),
                }
            }
            Ok(Out::ok(lines.join("\n"), json!({ "state": s, "sheet": sheet_path })))
        }
        RecordCmd::Frames { count, output, path, max_side } => {
            let rec = recd::recording_path(state, path.as_deref())?;
            let dur = video::duration(&rec)?;
            let n = (*count).clamp(1, 100);
            let times: Vec<f32> =
                (0..n).map(|k| if n == 1 { 0.0 } else { (dur - 0.05).max(0.0) * k as f32 / (n - 1) as f32 }).collect();
            std::fs::create_dir_all(output)?;
            let mut written: Vec<(f32, PathBuf)> = Vec::new();
            let mut last: Option<Vec<u8>> = None;
            for (t, png) in video::frames_at(&rec, &times, *max_side, None)? {
                // A short or cut-off recording maps several times to the
                // same still: keep one of each picture.
                if last.as_ref() == Some(&png) {
                    continue;
                }
                last = Some(png.clone());
                let p = output.join(format!("frame-{:02}-{:07.2}s.png", written.len(), t));
                std::fs::write(&p, png)?;
                written.push((t, p));
            }
            Ok(Out::ok(
                written.iter().map(|(t, p)| format!("t={t:.2}s {}", p.display())).collect::<Vec<_>>().join("\n"),
                json!({ "recording": rec, "frames": written.iter().map(|(t, p)| json!({ "t": t, "path": p })).collect::<Vec<_>>() }),
            ))
        }
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
/// an Xvfb had to be started (it stays, as after `xcua up`). A running
/// display is used whatever its size; an explicit `--size` that differs
/// gets a warning, since only `up --restart` changes it.
fn ensure(state: &State, display: Option<&str>, size: (u32, u32), size_explicit: bool) -> Result<String> {
    if let Some(d) = display {
        if !xvfb::answers(d) {
            bail!("no X server answers on {d}");
        }
        return Ok(d.to_string());
    }
    let up = xvfb::up(state, None, size, Owner::Up)?;
    if matches!(up, Up::Started { .. }) {
        eprintln!("{}", describe_up(&up));
    } else if size_explicit {
        if let Some((w, h)) = xvfb::display_size(up.display()).filter(|&s| s != size) {
            eprintln!(
                "xcua: {} runs at {w}x{h}, not {}x{}; using it as it is (`xcua up --size {}x{} --restart` changes it)",
                up.display(),
                size.0,
                size.1,
                size.0,
                size.1
            );
        }
    }
    Ok(up.display().to_string())
}

fn mcp(state: &State, display: Option<&str>, size: (u32, u32), shm: bool, max_side: Option<u32>, core_only: bool) -> Result<()> {
    let me = std::process::id();
    // Before the runtime: the Xvfb must not inherit tokio's threads' state.
    // With --display the state dir is not touched and nothing is released.
    let d = match display {
        Some(d) => ensure(state, Some(d), size, false)?,
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
            version: Some(env!("KMSCUA_BUILD").to_string()),
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

/// Never starts a display: with none running it says what `up` would
/// start.
fn doctor(state: &State, display: Option<&str>, size: (u32, u32)) -> Result<Out> {
    let mut out = xvfb::status(state);
    let found = match display {
        Some(d) => xvfb::answers(d).then(|| d.to_string()).ok_or_else(|| format!("no X server answers on {d}")),
        None => state.current().ok_or_else(|| "no display running".to_string()),
    };
    let d = match found {
        Ok(d) => d,
        Err(why) => {
            let xvfb_bin = xvfb::on_path("Xvfb");
            let wm = xvfb::find_wm();
            // status already said "display: none"; an explicit one gets its reason.
            if display.is_some() {
                out.push_str(&format!("display: {why}\n"));
            }
            out.push_str(&format!(
                "`xcua up` would start: Xvfb {} at {}x{}\n",
                xvfb_bin.as_ref().map(|p| p.display().to_string()).unwrap_or_else(|| "(NOT FOUND on PATH: install xvfb)".into()),
                size.0,
                size.1
            ));
            out.push_str("extensions: unknown until `xcua up`\n");
            out.push_str(&format!(
                "window manager for `up --wm auto`: {}\n",
                wm.unwrap_or("none installed (looked for jwm, openbox, fluxbox, icewm, twm)")
            ));
            return Ok(Out::ok(
                out.trim_end(),
                json!({ "ok": true, "display": null, "reason": why, "would_start": { "xvfb": xvfb_bin, "size": format!("{}x{}", size.0, size.1), "extensions": "unknown until up", "wm_auto": wm } }),
            ));
        }
    };
    let b = XBackend::open(&d, env_flag("XCUA_SHM"), &state.recordings())?;
    let r = b.doctor_report();
    let s = |k: &str| r[k].as_str().unwrap_or("?").to_string();
    let mut line = |l: String| {
        out.push_str(&l);
        out.push('\n');
    };
    line(format!("using display: {} ({})", s("display"), s("size")));
    if let Some(ext) = r["extensions"].as_object() {
        let list: Vec<String> = ext
            .iter()
            .map(|(k, v)| format!("{k} {}", if v.as_bool() == Some(true) { "yes" } else { "NO" }))
            .collect();
        line(format!("extensions: {}", list.join(", ")));
    }
    line(format!("capture: {}", s("capture")));
    line(format!(
        "window manager: {}",
        if r["window_manager"].as_bool() == Some(true) { "yes (it handles focus)" } else { "none (xcua focuses the clicked window)" }
    ));
    line(format!("spare keycodes: {}", r["spare_keycodes"]));
    line(format!("recording dir: {} (codec {})", s("record_dir"), xcua::backend::RECORD_CODEC));
    line("focus:".into());
    for l in s("focus").lines() {
        line(format!("  {l}"));
    }
    if let Some(notes) = r["notes"].as_array() {
        for n in notes {
            line(format!("note: {}", n.as_str().unwrap_or_default()));
        }
    }
    Ok(Out::ok(out.trim_end(), r))
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    #[test]
    fn cli_definition_is_consistent() {
        Cli::command().debug_assert();
    }

    /// A subcommand argument with a global's name (`batch`'s positional was
    /// once called `json`) panics only when parsed; debug_assert misses it.
    #[test]
    fn batch_and_global_json_parse_together() {
        let c = Cli::try_parse_from(["xcua", "--json", "batch", "[]"]).unwrap();
        assert!(c.json);
        assert!(matches!(&c.cmd, Cmd::Batch { actions, .. } if actions == "[]"));
    }

    #[test]
    fn text_may_start_with_a_hyphen() {
        let c = Cli::try_parse_from(["xcua", "type", "-5 degrees"]).unwrap();
        assert!(matches!(&c.cmd, Cmd::Type { text, delay_ms: 12 } if text == "-5 degrees"));
        let c = Cli::try_parse_from(["xcua", "record", "mark", "-x"]).unwrap();
        assert!(matches!(&c.cmd, Cmd::Record(RecordCmd::Mark { note }) if note == "-x"));
    }

    #[test]
    fn negative_scroll_parses() {
        let c = Cli::try_parse_from(["xcua", "scroll", "1", "2", "--dy", "-3"]).unwrap();
        assert!(matches!(c.cmd, Cmd::Scroll { dy: -3, .. }));
    }
}
