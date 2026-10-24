//! Find the user's graphical session environment when we were started from
//! somewhere else (an SSH shell, a systemd unit, an agent harness).
//!
//! Scans `/proc/*/environ` of processes owned by our uid and lifts the
//! variables a session-side tool needs (clipboard, AT-SPI). Nothing here is
//! required for screenshots or input: those go through cuad, below the
//! compositor.

use std::collections::HashMap;
use std::fs;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};

const WANTED: &[&str] = &[
    "WAYLAND_DISPLAY",
    "DISPLAY",
    "XDG_RUNTIME_DIR",
    "DBUS_SESSION_BUS_ADDRESS",
    "XDG_SESSION_TYPE",
    "XAUTHORITY",
];

pub fn hydrate() -> HashMap<String, String> {
    let mut found: HashMap<String, String> = HashMap::new();
    for k in WANTED {
        if let Ok(v) = std::env::var(k) {
            if !v.is_empty() {
                found.insert((*k).to_string(), v);
            }
        }
    }
    // A configured socket nobody listens on is worse than none: drop it so
    // the scan below can find the session that actually exists.
    if let Some(name) = found.get("WAYLAND_DISPLAY").cloned() {
        let runtime = std::env::var("XDG_RUNTIME_DIR").ok();
        if !wayland_socket_live(&name, runtime.as_deref()) {
            found.remove("WAYLAND_DISPLAY");
        }
    }
    let self_contained = (found.contains_key("WAYLAND_DISPLAY") || found.contains_key("DISPLAY"))
        && found.contains_key("DBUS_SESSION_BUS_ADDRESS");
    if !self_contained {
        let uid = unsafe { libc_getuid() };
        if let Ok(entries) = fs::read_dir("/proc") {
            // Collect every same-uid process's session vars, then merge
            // best-first. Compositors and session leaders (gnome-shell,
            // gnome-session-b) carry no display socket themselves, so rank by
            // what a process can actually contribute: Wayland clients beat
            // X11 clients (the native clipboard is better), a fuller
            // environment beats a sparser one, and among equals the oldest
            // pid wins (long-lived session processes carry the most).
            let mut candidates: Vec<(u8, u8, u32, HashMap<String, String>)> = Vec::new();
            for e in entries.flatten() {
                let Ok(pid) = e.file_name().to_string_lossy().parse::<u32>() else {
                    continue;
                };
                let Ok(status) = fs::read_to_string(format!("/proc/{pid}/status")) else {
                    continue;
                };
                let owner = status
                    .lines()
                    .find_map(|l| l.strip_prefix("Uid:"))
                    .and_then(|l| l.split_whitespace().next())
                    .and_then(|s| s.parse::<u32>().ok());
                if owner != Some(uid) {
                    continue;
                }
                let Ok(env) = fs::read(format!("/proc/{pid}/environ")) else {
                    continue;
                };
                let mut got: HashMap<String, String> = HashMap::new();
                for kv in env.split(|b| *b == 0) {
                    let s = String::from_utf8_lossy(kv);
                    if let Some((k, v)) = s.split_once('=') {
                        if WANTED.contains(&k) && !v.is_empty() {
                            got.insert(k.to_string(), v.to_string());
                        }
                    }
                }
                if got.is_empty() {
                    continue;
                }
                // A socket nobody listens on would anchor the gate below on a
                // logged-out session and hide the live one, so require a
                // connection and demote dead holders to display-less donors.
                if let Some(name) = got.get("WAYLAND_DISPLAY").map(String::as_str) {
                    let runtime = got.get("XDG_RUNTIME_DIR").map(String::as_str);
                    if !wayland_socket_live(name, runtime) {
                        got.remove("WAYLAND_DISPLAY");
                        if got.is_empty() {
                            continue;
                        }
                    }
                }
                let class = if got.contains_key("WAYLAND_DISPLAY") {
                    0
                } else if got.contains_key("DISPLAY") {
                    1
                } else {
                    2
                };
                let missing = WANTED.len() as u8 - got.len() as u8;
                candidates.push((class, missing, pid, got));
            }
            candidates.sort_by_key(|(class, missing, pid, _)| (*class, *missing, *pid));
            for (_, _, _, got) in candidates {
                // Once a Wayland socket is adopted, ignore candidates without
                // one: an env labelled x11 that carries no WAYLAND_DISPLAY
                // belongs to a different session, often an orphan from a
                // logged-out one (a surviving jobserver made doctor report
                // x11 on a Wayland desktop). Xwayland clients carry both
                // sockets, so they still contribute DISPLAY for pure-X11
                // clipboard helpers such as xsel.
                if found.contains_key("WAYLAND_DISPLAY") && !got.contains_key("WAYLAND_DISPLAY") {
                    continue;
                }
                for (k, v) in got {
                    found.entry(k).or_insert(v);
                }
                // Keep looking until both sockets are covered: some clients
                // carry only one of the two, and the native Wayland clipboard
                // is the better one, so neither kind alone completes the set.
                if found.contains_key("DBUS_SESSION_BUS_ADDRESS")
                    && found.contains_key("XDG_RUNTIME_DIR")
                    && found.contains_key("XDG_SESSION_TYPE")
                    && found.contains_key("WAYLAND_DISPLAY")
                    && found.contains_key("DISPLAY")
                {
                    break;
                }
            }
        }
    }
    // A Wayland socket is itself proof of a Wayland session; label it when no
    // client carried the name. DISPLAY alone proves nothing (it may be
    // Xwayland), so there is no x11 counterpart.
    if found.contains_key("WAYLAND_DISPLAY") && !found.contains_key("XDG_SESSION_TYPE") {
        found.insert("XDG_SESSION_TYPE".into(), "wayland".into());
    }
    found
}

/// A Wayland socket is only usable if something is listening on it. Absolute
/// paths stand alone; bare names resolve against the session's runtime dir.
fn wayland_socket_live(name: &str, runtime_dir: Option<&str>) -> bool {
    let path = if name.starts_with('/') {
        PathBuf::from(name)
    } else {
        match runtime_dir {
            Some(dir) => Path::new(dir).join(name),
            None => return false,
        }
    };
    UnixStream::connect(path).is_ok()
}

/// Apply the discovered variables to our own process environment.
pub fn apply(vars: &HashMap<String, String>) {
    for (k, v) in vars {
        if std::env::var(k).map(|s| s.is_empty()).unwrap_or(true) {
            std::env::set_var(k, v);
        }
    }
}

unsafe fn libc_getuid() -> u32 {
    extern "C" {
        fn getuid() -> u32;
    }
    getuid()
}
