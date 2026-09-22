//! Copy the submodule's safe wrapper (bindings/rust/libdrmtap/src/lib.rs)
//! into OUT_DIR minus inner attributes, so src/lib.rs can include! it.
use std::path::PathBuf;
fn main() {
    let src = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../third_party/libdrmtap/bindings/rust/libdrmtap/src/lib.rs");
    println!("cargo:rerun-if-changed={}", src.display());
    let body: String = std::fs::read_to_string(&src)
        .unwrap_or_else(|_| panic!("{} missing: git submodule update --init", src.display()))
        .lines()
        .filter(|l| { let t = l.trim_start(); !(t.starts_with("#![") || t.starts_with("//!")) })
        .map(|l| format!("{l}\n"))
        .collect();
    let out = PathBuf::from(std::env::var("OUT_DIR").unwrap()).join("wrapper.rs");
    std::fs::write(out, body).unwrap();
}
