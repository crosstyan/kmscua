//! Which physical button is the primary one for our virtual pointer.
//!
//! The daemon injects raw BTN_LEFT / BTN_RIGHT through uinput, and the
//! compositor's libinput applies the user's left-handed setting to that
//! device like to any other mouse. With it on, a raw BTN_LEFT arrives as a
//! secondary click: `left_click` on a dock icon opens its context menu.
//!
//! Agents mean "primary" when they say left (the vendor tool vocabulary has
//! no other word for it, and Windows tools honour SM_SWAPBUTTON the same
//! way), so the client asks the compositor and swaps before sending. The
//! wire protocol stays physical.
//!
//! Sources, first answer wins:
//! 1. `KMSCUA_BUTTONS=swapped|normal`, for setups nothing below can read.
//! 2. KWin: the live `leftHanded` of our device over D-Bus (per device).
//! 3. GNOME (Mutter on the bus): `org.gnome.desktop.peripherals.mouse
//!    left-handed`, which Mutter applies to every mouse including ours.
//! 4. sway: `swaymsg -t get_inputs`, our device's `libinput.left_handed`.
//! 5. Hyprland: `hyprctl getoption input:left_handed`.
//!
//! Without the `dbus` feature, 2 and 3 are skipped. A backend whose buttons
//! no desktop setting touches (an X server driven through XTEST) answers
//! through `Backend::handedness` instead and this is not asked at all.

use std::collections::HashMap;
use std::process::Command;

use kmscua_proto::Button;
use serde::Serialize;
#[cfg(feature = "dbus")]
use zbus::zvariant::OwnedValue;

/// Every name cuad gives its pointer contains this (`kmscua absolute
/// pointer`, `kmscua@seat1 absolute pointer`).
const POINTER_SUFFIX: &str = "absolute pointer";

#[derive(Debug, Clone, Serialize)]
pub struct Handedness {
    /// Primary is the physical right button.
    pub swapped: bool,
    /// Where the answer came from, for doctor.
    pub source: String,
}

impl Handedness {
    pub fn normal(source: impl Into<String>) -> Self {
        Self { swapped: false, source: source.into() }
    }

    /// The physical button that does what `logical` means: Left is the
    /// primary button, Right the secondary.
    pub fn physical(&self, logical: Button) -> Button {
        match (self.swapped, logical) {
            (true, Button::Left) => Button::Right,
            (true, Button::Right) => Button::Left,
            (_, b) => b,
        }
    }
}

pub async fn detect() -> Handedness {
    if let Ok(v) = std::env::var("KMSCUA_BUTTONS") {
        match v.to_ascii_lowercase().as_str() {
            "swapped" | "left-handed" => return Handedness { swapped: true, source: "KMSCUA_BUTTONS".into() },
            "normal" | "right-handed" => return Handedness::normal("KMSCUA_BUTTONS"),
            _ => log::warn!("KMSCUA_BUTTONS={v} ignored: want swapped or normal"),
        }
    }
    crate::session::apply(&crate::session::hydrate());
    #[cfg(feature = "dbus")]
    if let Ok(conn) = zbus::Connection::session().await {
        if let Some(h) = kwin(&conn).await {
            return h;
        }
        if has_owner(&conn, "org.gnome.Mutter.DisplayConfig").await {
            return gnome().unwrap_or_else(|| Handedness::normal("GNOME, gsettings unreadable; assuming right-handed"));
        }
    }
    if std::env::var_os("SWAYSOCK").is_some() {
        if let Some(h) = sway() {
            return h;
        }
    }
    if std::env::var_os("HYPRLAND_INSTANCE_SIGNATURE").is_some() {
        if let Some(h) = hyprland() {
            return h;
        }
    }
    Handedness::normal("no compositor setting found; assuming right-handed (set KMSCUA_BUTTONS=swapped to override)")
}

#[cfg(feature = "dbus")]
async fn has_owner(conn: &zbus::Connection, name: &str) -> bool {
    let Ok(dbus) = zbus::fdo::DBusProxy::new(conn).await else {
        return false;
    };
    let Ok(name) = zbus::names::BusName::try_from(name) else {
        return false;
    };
    dbus.name_has_owner(name).await.unwrap_or(false)
}

#[cfg(feature = "dbus")]
async fn get_prop(conn: &zbus::Connection, path: &str, iface: &str, prop: &str) -> Option<OwnedValue> {
    let reply = conn
        .call_method(
            Some("org.kde.KWin"),
            path,
            Some("org.freedesktop.DBus.Properties"),
            "Get",
            &(iface, prop),
        )
        .await
        .ok()?;
    reply.body().deserialize::<OwnedValue>().ok()
}

/// KWin keeps left-handed per device; ask for ours by name.
#[cfg(feature = "dbus")]
async fn kwin(conn: &zbus::Connection) -> Option<Handedness> {
    let names: Vec<String> = get_prop(
        conn,
        "/org/kde/KWin/InputDevice",
        "org.kde.KWin.InputDeviceManager",
        "devicesSysNames",
    )
    .await?
    .try_into()
    .ok()?;
    for sys in names {
        let path = format!("/org/kde/KWin/InputDevice/{sys}");
        let iface = "org.kde.KWin.InputDevice";
        let Some(name) = get_prop(conn, &path, iface, "name").await.and_then(|v| String::try_from(v).ok()) else {
            continue;
        };
        if !(name.starts_with("kmscua") && name.ends_with(POINTER_SUFFIX)) {
            continue;
        }
        let left = get_prop(conn, &path, iface, "leftHanded").await.and_then(|v| bool::try_from(v).ok())?;
        return Some(Handedness { swapped: left, source: format!("KWin device {sys} ({name}) leftHanded={left}") });
    }
    // KWin is running but has not picked our device up (cuad down?):
    // nothing it says applies to us, and the GNOME key would not either.
    Some(Handedness::normal("KWin lists no kmscua pointer; assuming right-handed"))
}

#[cfg(feature = "dbus")]
fn gnome() -> Option<Handedness> {
    let out = Command::new("gsettings")
        .args(["get", "org.gnome.desktop.peripherals.mouse", "left-handed"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let v = String::from_utf8_lossy(&out.stdout).trim().to_string();
    Some(Handedness {
        swapped: v == "true",
        source: format!("GNOME org.gnome.desktop.peripherals.mouse left-handed={v}"),
    })
}

fn sway() -> Option<Handedness> {
    let out = Command::new("swaymsg").args(["-t", "get_inputs", "-r"]).output().ok()?;
    let inputs: Vec<serde_json::Value> = serde_json::from_slice(&out.stdout).ok()?;
    let dev = inputs.iter().find(|i| {
        i["name"].as_str().is_some_and(|n| n.starts_with("kmscua") && n.ends_with(POINTER_SUFFIX))
    })?;
    let v = dev["libinput"]["left_handed"].as_str()?;
    Some(Handedness { swapped: v == "enabled", source: format!("sway input {} left_handed={v}", dev["identifier"]) })
}

fn hyprland() -> Option<Handedness> {
    let out = Command::new("hyprctl").args(["getoption", "input:left_handed", "-j"]).output().ok()?;
    let v: HashMap<String, serde_json::Value> = serde_json::from_slice(&out.stdout).ok()?;
    let on = v.get("int")?.as_i64()? != 0;
    Some(Handedness { swapped: on, source: format!("Hyprland input:left_handed={}", on as u8) })
}
