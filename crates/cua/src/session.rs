//! Find the user's graphical session environment when we were started from
//! somewhere else (an SSH shell, a systemd unit, an agent harness).
//!
//! Scans `/proc/*/environ` of processes owned by our uid and lifts the
//! variables a session-side tool needs (clipboard, AT-SPI). Nothing here is
//! required for screenshots or input: those go through cuad, below the
//! compositor.

use std::collections::HashMap;
use std::fs;

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
    if found.contains_key("WAYLAND_DISPLAY") || found.contains_key("DISPLAY") {
        if found.contains_key("DBUS_SESSION_BUS_ADDRESS") {
            return found;
        }
    }
    let uid = unsafe { libc_getuid() };
    let Ok(entries) = fs::read_dir("/proc") else {
        return found;
    };
    // Prefer long-lived session processes; they carry the full environment.
    let preferred = ["gnome-shell", "gnome-session-b", "plasmashell", "xfce4-session", "sway", "Hyprland", "niri", "kwin_wayland"];
    let mut candidates: Vec<(u8, u32)> = Vec::new();
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
        let comm = fs::read_to_string(format!("/proc/{pid}/comm")).unwrap_or_default();
        let rank = if preferred.iter().any(|p| comm.trim() == *p) { 0 } else { 1 };
        candidates.push((rank, pid));
    }
    candidates.sort();
    for (_, pid) in candidates {
        let Ok(env) = fs::read(format!("/proc/{pid}/environ")) else {
            continue;
        };
        let mut got = HashMap::new();
        for kv in env.split(|b| *b == 0) {
            let s = String::from_utf8_lossy(kv);
            if let Some((k, v)) = s.split_once('=') {
                if WANTED.contains(&k) && !v.is_empty() {
                    got.insert(k.to_string(), v.to_string());
                }
            }
        }
        if got.contains_key("WAYLAND_DISPLAY") || got.contains_key("DISPLAY") {
            for (k, v) in got {
                found.entry(k).or_insert(v);
            }
            // Keep looking until we have a Wayland socket too: Xwayland
            // clients only carry DISPLAY, and the native clipboard is better.
            if found.contains_key("DBUS_SESSION_BUS_ADDRESS")
                && found.contains_key("WAYLAND_DISPLAY")
            {
                break;
            }
        }
    }
    found
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
