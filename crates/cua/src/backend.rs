//! The seam between the MCP layer and whatever does the work.
//!
//! `cua` talks to cuad over its socket; another backend (an in-process X11
//! server, a test double) answers the same `kmscua_proto::Request`s itself.
//! Everything above this trait, the tool vocabulary, the coordinate view,
//! the waits and the recording timeline, is shared.

use std::time::Duration;

use anyhow::Result;
use kmscua_proto::{Client, Reply, Request, Status};

use crate::handedness::Handedness;

/// Answers `kmscua_proto` requests. Called from blocking threads, never from
/// the async runtime, so an implementation may block.
///
/// The contract is cuad's: every coordinate is a source pixel (the space of
/// a native screenshot), a request that presses something releases it before
/// it answers, and a failed request is `Err`, never an `Ok` reply with
/// `header.ok == false`. A `Screenshot` reply carries a `ScreenshotInfo` in
/// `header.data` and the encoded image as `payload`; `Record*` replies carry
/// a `RecordInfo` whose `path` is an MP4 or a stills directory (see
/// [`crate::stills`]).
pub trait Backend: Send + Sync {
    fn call(&self, req: &Request) -> Result<Reply>;

    /// `call` for a request that may run longer than the usual timeout
    /// (`KeyHold`). Backends without a timeout keep the default.
    fn call_timeout(&self, req: &Request, timeout: Duration) -> Result<Reply> {
        let _ = timeout;
        self.call(req)
    }

    /// Whether `Request::Type` takes any Unicode text. When true,
    /// [`crate::type_text`] sends the whole string in one request instead of
    /// pasting the non-ASCII runs through the clipboard. cuad's US keymap
    /// says no; an X server can bind a spare keycode to any keysym.
    fn types_unicode(&self) -> bool {
        false
    }

    /// Text for `get_focused`. `None` (the default) falls back to AT-SPI
    /// when the `atspi` feature is on, and to an "unavailable" error when it
    /// is off.
    fn focused(&self) -> Option<Result<String>> {
        None
    }

    /// Which physical button is primary. `None` (the default) asks the
    /// desktop ([`crate::handedness::detect`]); a backend whose buttons no
    /// desktop setting remaps answers itself.
    fn handedness(&self) -> Option<Handedness> {
        None
    }

    /// Report for the `doctor` tool, with the button mapping the MCP layer
    /// just detected. The default renders `Status`.
    fn doctor(&self, buttons: Option<Handedness>) -> String {
        let v = match self.call(&Request::Status).and_then(|r| r.data::<Status>()) {
            Ok(s) => serde_json::json!({ "daemon": s, "buttons": buttons }),
            Err(e) => serde_json::json!({ "daemon": null, "daemon_error": format!("{e:#}"), "buttons": buttons }),
        };
        serde_json::to_string_pretty(&v).unwrap_or_default()
    }
}

impl Backend for Client {
    fn call(&self, req: &Request) -> Result<Reply> {
        Client::call(self, req)
    }

    fn call_timeout(&self, req: &Request, timeout: Duration) -> Result<Reply> {
        self.with_timeout(timeout).call(req)
    }

    fn doctor(&self, buttons: Option<Handedness>) -> String {
        let mut d = crate::doctor(self);
        d.buttons = buttons;
        serde_json::to_string_pretty(&d).unwrap_or_default()
    }
}
