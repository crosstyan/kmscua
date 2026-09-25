//! Safe wrapper for libdrmtap; the body is the submodule's own crate source,
//! included at build time so the submodule stays pristine. See build.rs.
#![allow(dead_code)]
include!(concat!(env!("OUT_DIR"), "/wrapper.rs"));

impl Frame {
    /// KMS framebuffer id; changes on a page flip to another buffer. The
    /// submodule's wrapper does not expose it (see build.rs for why it is
    /// included rather than edited).
    pub fn fb_id(&self) -> u32 {
        self.raw.fb_id
    }
}
