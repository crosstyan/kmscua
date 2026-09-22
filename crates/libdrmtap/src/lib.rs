//! Safe wrapper for libdrmtap; the body is the submodule's own crate source,
//! included at build time so the submodule stays pristine. See build.rs.
#![allow(dead_code)]
include!(concat!(env!("OUT_DIR"), "/wrapper.rs"));
