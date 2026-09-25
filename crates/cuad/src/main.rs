//! cuad: the privileged half of kmscua.
//!
//! Runs as root (or with CAP_SYS_ADMIN for the DRM side and rw /dev/uinput).
//! Owns one libdrmtap context and two uinput devices for its whole lifetime,
//! and answers requests on a unix socket. It never talks to a compositor.

mod capture;
mod input;
#[cfg(feature = "jetson")]
mod jetson;
mod keymap;
mod recorder;
mod server;

use std::path::PathBuf;

use anyhow::{Context, Result};
use clap::Parser;

#[derive(Parser, Debug)]
#[command(name = "cuad", version, about)]
struct Args {
    /// Unix socket to listen on.
    #[arg(long, default_value = kmscua_proto::DEFAULT_SOCKET)]
    socket: PathBuf,
    /// Group that may connect (socket is chgrp'd to it, mode 0660).
    #[arg(long, default_value = "kmscua")]
    group: String,
    /// Do not chgrp the socket (keep it root-only, for testing with sudo).
    #[arg(long)]
    no_group: bool,
    /// Also chown the socket to this user, so they can connect before their
    /// next login picks up the group (env KMSCUA_OWNER).
    #[arg(long, env = "KMSCUA_OWNER")]
    owner: Option<String>,
    /// DRM device (default: auto-detect the card with an active CRTC).
    #[arg(long)]
    device: Option<String>,
    /// CRTC id to capture (default: first active).
    #[arg(long, default_value_t = 0)]
    crtc: u32,
    /// libdrmtap privilege helper, for running without CAP_SYS_ADMIN.
    #[arg(long)]
    helper: Option<String>,
    /// Name the uinput devices `kmscua@<seat> …` so a udev rule can give them
    /// to a compositor on another seat (the virtual desktop). Default: the
    /// plain names, which land on seat0 like a physical keyboard.
    #[arg(long, env = "KMSCUA_SEAT")]
    seat: Option<String>,
    /// Disable screen recording (on by default when a GStreamer H.264
    /// encoder is present).
    #[arg(long)]
    no_record: bool,
    /// Force an encoder: jetson-zc (Jetson zero-copy, needs the `jetson` build
    /// feature), nvv4l2h264enc (Jetson), nvh264enc (NVENC), x264enc, openh264enc.
    #[arg(long, env = "KMSCUA_RECORD_CODEC")]
    record_codec: Option<String>,
    /// Where recordings are written.
    #[arg(long, default_value = "/var/lib/kmscua/recordings", env = "KMSCUA_RECORD_DIR")]
    record_dir: PathBuf,
    /// Probe capture and input, print status JSON, exit.
    #[arg(long)]
    check: bool,
}

fn main() -> Result<()> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();
    let args = Args::parse();

    let mut capture = capture::Capturer::open(args.device.clone(), args.crtc, args.helper.clone())
        .context("open DRM capture")?;
    let desktop = capture.desktop_rect().context("determine desktop rect")?;
    log::info!(
        "desktop {}x{} at ({},{}), gpu driver {:?}, cursor {}",
        desktop.width,
        desktop.height,
        desktop.x,
        desktop.y,
        capture.gpu_driver(),
        if capture.cursor_supported() { "ok" } else { "unavailable" }
    );
    let input = input::Input::create(desktop, args.seat.as_deref()).context("create uinput devices")?;
    // Encoder detection shells out to gst-inspect, which rebuilds the
    // GStreamer registry on first run as root (25 s measured). Do it off
    // the startup path; recording requests before it finishes are refused
    // with "detecting".
    let codec_slot: server::CodecSlot = Default::default();
    let zero_copy = !args.no_record && recorder::zero_copy_probe(&mut capture, args.record_codec.as_deref());
    if zero_copy {
        log::info!("recording: {} (VIC + NVENC from the scanout dma-buf) -> {}", recorder::ZERO_COPY, args.record_dir.display());
    }
    if args.record_codec.as_deref() == Some(recorder::ZERO_COPY) {
        // Forced: no GStreamer fallback, and no gst-inspect run.
        *codec_slot.lock().unwrap() = Some(None);
    } else if !args.no_record {
        let slot = codec_slot.clone();
        let preferred = args.record_codec.clone();
        let dir = args.record_dir.clone();
        std::env::set_var("GST_REGISTRY", dir.join("../gst-registry.bin"));
        std::thread::Builder::new()
            .name("codec-detect".into())
            .spawn(move || {
                let c = recorder::detect(preferred.as_deref());
                match c {
                    Some(c) => log::info!(
                        "recording: {} ({}) -> {}",
                        c.name(),
                        if c.hardware() { "hardware" } else { "software" },
                        dir.display()
                    ),
                    None => log::warn!("recording: no usable GStreamer H.264 encoder, disabled"),
                }
                *slot.lock().unwrap() = Some(c);
            })
            .expect("spawn codec detection");
    } else {
        *codec_slot.lock().unwrap() = Some(None);
    }
    let owner_uid = match args.owner.as_deref() {
        Some(u) => Some(server::lookup_uid(u)?),
        None => None,
    };
    let rec = server::RecordConfig {
        codec: codec_slot,
        dir: args.record_dir.clone(),
        owner_uid,
        zero_copy,
    };
    let mut daemon = server::Daemon::new(capture, input, rec);

    if args.check {
        let (info, bytes) = daemon.screenshot(Some(1280))?;
        let out = std::env::temp_dir().join("cuad-check.png");
        std::fs::write(&out, &bytes)?;
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "screenshot": info,
                "written": out,
                "desktop": desktop,
            }))?
        );
        daemon.input.release_all();
        return Ok(());
    }

    let group = if args.no_group { None } else { Some(args.group.as_str()) };
    let result = daemon.serve(&args.socket, group, args.owner.as_deref());
    daemon.input.release_all();
    result
}
