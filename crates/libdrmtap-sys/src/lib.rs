//! FFI for libdrmtap. The declarations are the submodule's own
//! `bindings/rust/libdrmtap-sys/src/lib.rs`, included at build time; see build.rs.
#![allow(non_camel_case_types, non_upper_case_globals, dead_code)]
include!(concat!(env!("OUT_DIR"), "/ffi.rs"));
