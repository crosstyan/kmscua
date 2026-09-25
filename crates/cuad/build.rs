//! With the `jetson` feature, compile the NvBufSurface / NVENC shim and link
//! the L4T libraries. Nothing to do otherwise.

fn main() {
    #[cfg(feature = "jetson")]
    jetson();
}

#[cfg(feature = "jetson")]
fn jetson() {
    let include = std::env::var("JETSON_MMAPI_INCLUDE")
        .unwrap_or_else(|_| "/usr/src/jetson_multimedia_api/include".to_owned());
    let libdir = std::env::var("JETSON_LIB_DIR")
        .unwrap_or_else(|_| "/usr/lib/aarch64-linux-gnu/nvidia".to_owned());
    println!("cargo:rerun-if-env-changed=JETSON_MMAPI_INCLUDE");
    println!("cargo:rerun-if-env-changed=JETSON_LIB_DIR");
    println!("cargo:rerun-if-changed=src/jetson/nv.c");
    cc::Build::new()
        .file("src/jetson/nv.c")
        .include(&include)
        .warnings(false)
        .compile("jetson_nv");
    println!("cargo:rustc-link-search=native={libdir}");
    for lib in ["nvbufsurface", "nvbufsurftransform", "v4l2"] {
        println!("cargo:rustc-link-lib=dylib={lib}");
    }
}
