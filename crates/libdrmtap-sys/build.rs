//! Build libdrmtap from the submodule at third_party/libdrmtap.
//!
//! The submodule stays pristine: its C sources are copied into OUT_DIR, the
//! patches under patches/ are applied there with `patch -p1`, and that copy
//! is compiled. The FFI declarations are the submodule's own lib.rs, copied
//! with its inner attributes stripped so it can be `include!`d.

use std::path::{Path, PathBuf};
use std::process::Command;

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..").canonicalize().unwrap()
}

fn copy_tree(from: &Path, to: &Path) {
    std::fs::create_dir_all(to).unwrap();
    for e in std::fs::read_dir(from).unwrap().flatten() {
        let p = e.path();
        let dst = to.join(e.file_name());
        if p.is_dir() {
            copy_tree(&p, &dst);
        } else {
            std::fs::copy(&p, &dst).unwrap();
        }
    }
}

fn main() {
    let root = repo_root();
    let sub = root.join("third_party/libdrmtap");
    if !sub.join("src/drmtap.c").exists() {
        panic!(
            "third_party/libdrmtap is empty: run `git submodule update --init` in {}",
            root.display()
        );
    }
    let out = PathBuf::from(std::env::var("OUT_DIR").unwrap());
    let work = out.join("libdrmtap");
    let _ = std::fs::remove_dir_all(&work);
    copy_tree(&sub.join("src"), &work.join("src"));
    copy_tree(&sub.join("include"), &work.join("include"));
    println!("cargo:rerun-if-changed={}", sub.join("src").display());
    println!("cargo:rerun-if-changed={}", sub.join("include").display());

    // Apply every patch in patches/ that names libdrmtap.
    let patches = root.join("patches");
    println!("cargo:rerun-if-changed={}", patches.display());
    let mut names: Vec<PathBuf> = std::fs::read_dir(&patches)
        .map(|d| {
            d.flatten()
                .map(|e| e.path())
                .filter(|p| {
                    p.file_name()
                        .and_then(|n| n.to_str())
                        .map(|n| n.starts_with("libdrmtap-") && n.ends_with(".patch"))
                        .unwrap_or(false)
                })
                .collect()
        })
        .unwrap_or_default();
    names.sort();
    for p in names {
        println!("cargo:rerun-if-changed={}", p.display());
        let status = Command::new("patch")
            .args(["-p1", "-s", "-N", "-d"])
            .arg(&work)
            .arg("-i")
            .arg(&p)
            .status()
            .expect("`patch` is required to build libdrmtap-sys (apt install patch)");
        assert!(status.success(), "failed to apply {}", p.display());
    }

    // FFI declarations from the submodule, minus inner attributes.
    let ffi_src = sub.join("bindings/rust/libdrmtap-sys/src/lib.rs");
    println!("cargo:rerun-if-changed={}", ffi_src.display());
    let ffi: String = std::fs::read_to_string(&ffi_src)
        .unwrap()
        .lines()
        .filter(|l| { let t = l.trim_start(); !(t.starts_with("#![") || t.starts_with("//!")) })
        .map(|l| format!("{l}\n"))
        .collect();
    std::fs::write(out.join("ffi.rs"), ffi).unwrap();

    let csrc = work.join("src");
    let mut build = cc::Build::new();
    build
        .files(
            [
                "drmtap.c",
                "drm_enumerate.c",
                "drm_grab.c",
                "privilege_helper.c",
                "pixel_convert.c",
                "cursor.c",
                "gpu_egl.c",
                "gpu_intel.c",
                "gpu_amd.c",
                "gpu_nvidia.c",
                "gpu_generic.c",
            ]
            .iter()
            .map(|f| csrc.join(f)),
        )
        .include(&csrc)
        .include(work.join("include"))
        .define("_POSIX_C_SOURCE", "200809L")
        .define("HAVE_EGL", "1")
        .define("HAVE_SECCOMP", "1")
        .define("HAVE_LIBCAP", "1")
        .flag("-std=c11")
        .flag("-Wno-unused-parameter")
        .flag("-Wno-sign-compare");
    // The sources define _GNU_SOURCE themselves; defining it here too only
    // produces a redefinition warning per file.
    let drm_includes: Vec<PathBuf> = match pkg_config::Config::new().cargo_metadata(false).probe("libdrm") {
        Ok(lib) => lib.include_paths,
        Err(_) => vec![PathBuf::from("/usr/include/libdrm")],
    };
    for inc in &drm_includes {
        build.include(inc);
    }
    if probe_hdr_metadata(&build, &drm_includes, &out) {
        build.define("HAVE_HDR_METADATA", "1");
    }
    build.compile("drmtap");
    println!("cargo:rustc-link-lib=drm");
    println!("cargo:rustc-link-lib=seccomp");
    println!("cargo:rustc-link-lib=cap");
    println!("cargo:rustc-link-lib=m");
    println!("cargo:rustc-link-lib=dl");
}

fn probe_hdr_metadata(build: &cc::Build, drm_includes: &[PathBuf], out: &Path) -> bool {
    let test_c = out.join("hdr_probe.c");
    let src = "#include <xf86drmMode.h>\n#include <drm_mode.h>\nint main(void){struct hdr_output_metadata m;const struct hdr_metadata_infoframe *i=&m.hdmi_metadata_type1;(void)sizeof(m);(void)i;return 0;}\n";
    if std::fs::write(&test_c, src).is_err() {
        return false;
    }
    let mut cmd = build.get_compiler().to_command();
    cmd.arg("-fsyntax-only");
    for inc in drm_includes {
        cmd.arg("-I").arg(inc);
    }
    cmd.arg(&test_c);
    matches!(cmd.status(), Ok(s) if s.success())
}
