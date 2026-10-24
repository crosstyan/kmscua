//! Recording as a directory of JPEG stills, in the layout
//! `kmscua_mcp::stills` reads (see its module doc for the contract).
//!
//! INTERFACE STUB: fixed by the orchestrator so the X11 backend and the
//! recorder can be written in parallel. The recorder author replaces the
//! bodies; the signatures stay.

use std::path::Path;

use anyhow::{bail, Result};
use image::RgbaImage;
use kmscua_proto::RecordInfo;

/// Grabs one full frame of the display, cursor composited when the flag is set.
/// Called from the recorder's own thread.
pub type Grab = Box<dyn FnMut(bool) -> Result<RgbaImage> + Send>;

/// The `Request::RecordStart` fields, unpacked.
#[derive(Debug, Clone, Default)]
pub struct RecordOpts {
    pub name: Option<String>,
    pub fps: Option<u32>,
    pub max_side: Option<u32>,
    pub max_seconds: Option<u32>,
    pub cursor: Option<bool>,
}

pub struct Recorder {}

impl Recorder {
    /// Start a capture thread writing `<dir>/<name>.frames/<ms>.jpg`.
    pub fn start(dir: &Path, opts: RecordOpts, grab: Grab) -> Result<Self> {
        let _ = (dir, opts, grab);
        bail!("recording not implemented yet")
    }

    /// Current state; `recording: false` once stopped or past `max_seconds`.
    pub fn info(&self) -> RecordInfo {
        unimplemented!()
    }

    /// Stop, wait for the last still to be on disk, return the final info
    /// with `finalized: Some(true)`.
    pub fn stop(&mut self) -> RecordInfo {
        unimplemented!()
    }
}
