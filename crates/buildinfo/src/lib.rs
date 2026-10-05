//! Called from the binaries' `build.rs`: stamps the build so a running
//! `cuad` and the `cua` talking to it can be told apart from the ones in
//! `target/`. Sets, for `env!()` in the crate being built:
//!
//! - `KMSCUA_BUILD_ID`: `<12-hex git hash>[-dirty]`, or `unknown` outside a
//!   checkout. Two binaries from the same tree share it, so it is what
//!   `cua doctor` compares.
//! - `KMSCUA_BUILD`: the long form for `--version`:
//!   `0.1.0 (47a3e08415c1-dirty, 2026-10-05T21:02Z, release, x86_64-unknown-linux-gnu)`.
//!
//! The dirty flag covers tracked files only. `SOURCE_DATE_EPOCH` is honored
//! for reproducible builds. No dependencies, so it adds nothing to the
//! build-dependency graph but itself.

use std::path::Path;
use std::process::Command;

pub fn emit() {
    let manifest_dir = std::env::var("CARGO_MANIFEST_DIR").unwrap_or_default();
    let pkg_version = std::env::var("CARGO_PKG_VERSION").unwrap_or_default();
    let profile = std::env::var("PROFILE").unwrap_or_default();
    let target = std::env::var("TARGET").unwrap_or_default();

    // Re-run when the sources, the manifest or the git state change. Naming
    // any path here turns off cargo's "any file in the package" default, so
    // the package's own files are listed explicitly.
    println!("cargo:rerun-if-changed=Cargo.toml");
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=src");
    println!("cargo:rerun-if-env-changed=SOURCE_DATE_EPOCH");

    let id = match git(&manifest_dir, &["rev-parse", "--short=12", "HEAD"]) {
        Some(hash) => {
            if let Some(git_dir) = git(&manifest_dir, &["rev-parse", "--absolute-git-dir"]) {
                for f in ["HEAD", "index", "packed-refs"] {
                    rerun_if_exists(Path::new(&git_dir).join(f));
                }
                // The branch's own ref file, so a commit on it re-stamps.
                if let Some(r) = git(&manifest_dir, &["symbolic-ref", "-q", "HEAD"]) {
                    rerun_if_exists(Path::new(&git_dir).join(r));
                }
            }
            let dirty = git(&manifest_dir, &["status", "--porcelain", "--untracked-files=no"])
                .map(|s| !s.is_empty())
                .unwrap_or(false);
            if dirty {
                format!("{hash}-dirty")
            } else {
                hash
            }
        }
        None => "unknown".to_string(),
    };

    let epoch = std::env::var("SOURCE_DATE_EPOCH")
        .ok()
        .and_then(|s| s.parse::<i64>().ok())
        .unwrap_or_else(|| {
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs() as i64)
                .unwrap_or(0)
        });
    let when = iso_minute_utc(epoch);

    println!("cargo:rustc-env=KMSCUA_BUILD_ID={id}");
    println!("cargo:rustc-env=KMSCUA_BUILD={pkg_version} ({id}, {when}, {profile}, {target})");
}

fn rerun_if_exists(p: std::path::PathBuf) {
    if p.exists() {
        println!("cargo:rerun-if-changed={}", p.display());
    }
}

fn git(dir: &str, args: &[&str]) -> Option<String> {
    let out = Command::new("git").args(args).current_dir(dir).output().ok()?;
    if !out.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// `YYYY-MM-DDTHH:MMZ` from unix seconds (Howard Hinnant's civil-from-days).
fn iso_minute_utc(epoch: i64) -> String {
    let days = epoch.div_euclid(86_400);
    let secs = epoch.rem_euclid(86_400);
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    format!("{y:04}-{m:02}-{d:02}T{:02}:{:02}Z", secs / 3600, (secs % 3600) / 60)
}

#[cfg(test)]
mod tests {
    use super::iso_minute_utc;

    #[test]
    fn dates() {
        assert_eq!(iso_minute_utc(0), "1970-01-01T00:00Z");
        assert_eq!(iso_minute_utc(951_782_400), "2000-02-29T00:00Z");
        assert_eq!(iso_minute_utc(1_791_234_120), "2026-10-05T21:02Z");
    }
}
