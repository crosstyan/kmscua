//! xcua: kmscua's MCP tools over an X server it owns (or attaches to).
//!
//! One unprivileged process, no daemon and no socket of its own: `xcua mcp`
//! serves `kmscua_mcp` on stdio with [`backend::XBackend`] answering the
//! requests in-process over x11rb's pure-Rust connection. Made for sandboxes
//! that have no GPU, DRM or uinput: an Xvfb is the screen.

pub mod backend;
pub mod cli;
pub mod input;
pub mod keysym;
pub mod pixels;
pub mod recd;
pub mod record;
pub mod view;
pub mod x11;
pub mod xvfb;

pub use backend::XBackend;
