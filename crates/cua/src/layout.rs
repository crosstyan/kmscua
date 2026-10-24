//! The compositor's monitor layout, read over D-Bus and pushed to cuad.
//!
//! DRM tells the daemon each CRTC's mode and nothing about where the
//! monitors sit next to each other or at what scale; that lives in the
//! compositor. This module asks it (Mutter today; the request shape is
//! compositor-neutral so KDE and X11 readers can be added beside it) and
//! sends the answer to the daemon, which arranges capture and input from it.
//!
//! Only the rectangles' proportions and relative positions matter to the
//! daemon, so logical pixels are passed as Mutter reports them.

use std::collections::HashMap;

use anyhow::{anyhow, bail, Context, Result};
use kmscua_proto::{Client, LayoutMonitor, Request, Status};
use serde::Deserialize;
use zbus::zvariant::{OwnedValue, Type};

// org.gnome.Mutter.DisplayConfig.GetCurrentState:
//   (u serial,
//    a((ssss) a(siiddada{sv}) a{sv}) monitors,
//    a(iiduba(ssss)a{sv})           logical_monitors,
//    a{sv}                          properties)
// The structs mirror the wire shape field for field, so some fields are
// decoded and never read.

/// connector, vendor, product, serial
#[derive(Deserialize, Type)]
#[allow(dead_code)]
struct MonitorSpec(String, String, String, String);

/// id, width, height, refresh, preferred scale, supported scales, properties
#[derive(Deserialize, Type)]
#[allow(dead_code)]
struct Mode(String, i32, i32, f64, f64, Vec<f64>, HashMap<String, OwnedValue>);

#[derive(Deserialize, Type)]
#[allow(dead_code)]
struct Monitor(MonitorSpec, Vec<Mode>, HashMap<String, OwnedValue>);

/// x, y, scale, transform, primary, monitors (several when mirrored), properties
#[derive(Deserialize, Type)]
#[allow(dead_code)]
struct LogicalMonitor(i32, i32, f64, u32, bool, Vec<MonitorSpec>, HashMap<String, OwnedValue>);

type State = (u32, Vec<Monitor>, Vec<LogicalMonitor>, HashMap<String, OwnedValue>);

/// The layout as Mutter (GNOME Shell) sees it, in logical pixels.
pub async fn query_mutter() -> Result<Vec<LayoutMonitor>> {
    let env = crate::session::hydrate();
    crate::session::apply(&env);
    let conn = zbus::Connection::session()
        .await
        .context("connect to the session bus (is a graphical session running for this user?)")?;
    let reply = conn
        .call_method(
            Some("org.gnome.Mutter.DisplayConfig"),
            "/org/gnome/Mutter/DisplayConfig",
            Some("org.gnome.Mutter.DisplayConfig"),
            "GetCurrentState",
            &(),
        )
        .await
        .context("org.gnome.Mutter.DisplayConfig.GetCurrentState (only GNOME is supported so far)")?;
    let (_, monitors, logical, props): State = reply.body().deserialize().context("decode GetCurrentState")?;
    // layout-mode 1 = logical (positions and sizes in logical px), 2 = physical.
    let physical = props.get("layout-mode").and_then(|v| u32::try_from(v.clone()).ok()) == Some(2);

    let mut out = Vec::new();
    for LogicalMonitor(x, y, scale, transform, primary, specs, _) in logical {
        for (i, spec) in specs.iter().enumerate() {
            let connector = spec.0.clone();
            let m = monitors
                .iter()
                .find(|m| m.0 .0 == connector)
                .ok_or_else(|| anyhow!("logical monitor names {connector}, which is not in the monitor list"))?;
            let mode = m
                .1
                .iter()
                .find(|md| md.6.get("is-current").and_then(|v| bool::try_from(v.clone()).ok()).unwrap_or(false))
                .or_else(|| m.1.first())
                .ok_or_else(|| anyhow!("{connector} has no modes"))?;
            let (mut w, mut h) = (mode.1 as f64, mode.2 as f64);
            // 1, 3, 5, 7 are 90 and 270 degrees, plain and flipped.
            if transform % 2 == 1 {
                std::mem::swap(&mut w, &mut h);
            }
            if !physical && scale > 0.0 {
                w /= scale;
                h /= scale;
            }
            // Mirrored outputs share one logical rectangle; the daemon draws
            // each into the same place.
            out.push(LayoutMonitor {
                connector,
                x,
                y,
                width: w.round().max(1.0) as u32,
                height: h.round().max(1.0) as u32,
                primary: primary && i == 0,
            });
        }
    }
    if out.is_empty() {
        bail!("Mutter reports no logical monitors (all outputs off?)");
    }
    Ok(out)
}

/// Read the layout from the compositor and push it to the daemon.
pub async fn push(c: &Client) -> Result<Status> {
    let monitors = query_mutter().await?;
    set(c, monitors).await
}

/// Push a layout; an empty one resets the daemon to its single-display default.
pub async fn set(c: &Client, monitors: Vec<LayoutMonitor>) -> Result<Status> {
    let c = c.clone();
    tokio::task::spawn_blocking(move || c.call(&Request::SetLayout { monitors })?.data::<Status>()).await?
}

/// One line per display, for humans.
pub fn describe(s: &Status) -> String {
    let mut lines = vec![format!(
        "desktop {}x{} at ({},{}), layout {}",
        s.desktop.width,
        s.desktop.height,
        s.desktop.x,
        s.desktop.y,
        s.layout.as_deref().unwrap_or("none (single-display default)")
    )];
    for d in &s.displays {
        lines.push(match d.desktop {
            Some(r) => format!(
                "  {:<10} {}x{}@{}  -> desktop ({},{}) {}x{}  scale {:.3}",
                d.name, d.width, d.height, d.refresh_hz, r.x, r.y, r.width, r.height, d.scale
            ),
            None => format!("  {:<10} {}x{}@{}  -> not placed", d.name, d.width, d.height, d.refresh_hz),
        });
    }
    lines.join("\n")
}
