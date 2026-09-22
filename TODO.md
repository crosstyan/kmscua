# TODO

Ordered by value. Estimates are for one person who knows the codebase.

## Codex profile (app-scoped, accessibility-first) — about 2 days

Not started, on purpose: current vision models do not need the tree and are not trained
on it (see the README's "Not yet"). Build it only for a model that is. The wire shapes
are in [docs/vendor-tool-shapes.md](docs/vendor-tool-shapes.md) section 3; nothing below
needs more research. `crates/cua/src/atspi.rs` already has the connection, the app and
window listing, the cached tree walk and the focused-node search.

1. Tree snapshot with a node budget, `do_action`, `EditableText` set/insert, `Text`
   selection, `Component` extents (window-relative on Wayland; scanout mapping needs the
   window origin, which needs the compositor). About half a day on top of `atspi.rs`.
   The MIT `atspi_tree.rs` in agent-sh/computer-use-linux is a good reference.
2. Text renderer for the Codex tree grammar (`<index> <role> (<states>) <title>, Value:…,
   Secondary Actions:…`, four-space indent, `The focused UI element is N …`), with the
   diff mode and `disableDiff`.
3. `cua mcp --profile codex` exposing `list_apps, get_app_state, click, drag, scroll,
   type_text, set_value, select_text, press_key, perform_secondary_action` with `app` on
   every call, `element_index` as a string, window-cropped screenshots, and a refreshed
   tree returned from every action.
4. Window geometry without a compositor API: AT-SPI window extents for the crop, and
   `_NET_ACTIVE_WINDOW` via Xwayland as a fallback for focus. Native-Wayland-only apps
   with no AT-SPI tree fall back to coordinates.

## Claude Code parity — about half a day

- `request_access {apps, reason}` / `list_granted_applications` / `open_application`:
  an allowlist of app names checked against the focused AT-SPI application before
  mutating actions, with the vendor error text. `atspi.rs` already knows the active app.
- `read_clipboard` / `write_clipboard` via `wl-paste`/`wl-copy` (already used for
  non-ASCII typing).
- `switch_display` once multi-CRTC capture exists.

## Capture

- Multi-CRTC: grab every active CRTC and stitch into the union rect. `displays` already
  lists them; `plan_size` and the ABS range already assume the union.
- Blanked output: detect an all-black frame and nudge (`Wake`) before retrying, instead
  of returning black.
- `record_frames` extracts each still with its own ffmpeg seek (about 0.3 s each);
  one pass with a `select` filter would make a 30-frame sheet several times faster.
- Cursor hotspot: libdrmtap reports `hot_x/hot_y` 0 on Tegra, so `cursor_position` is
  off by the cursor's hotspot (6-14 px scanout). Either read the hotspot from the DRM
  plane properties where the driver exposes them, or report the last injected position.

## Input

- xkb-aware typing: map characters through the active layout instead of assuming US;
  keep the clipboard fallback for anything the layout cannot produce.
- Touch/pen: a second uinput device with `BTN_TOUCH` for apps that only take touch.

## Privilege

- Package the libdrmtap helper split (`--helper`, `cap_sys_admin+ep` on a 0750 binary
  owned by a dedicated group) so `cuad` can drop root; keep a udev rule for
  `/dev/uinput`.
- Tighten the systemd unit again once the Tegra device set is known
  (`DevicePolicy=closed` broke the EGL detile silently).

## Packaging

- Debian package with the systemd unit, group creation and the drop-in.
- `uinput.ko` DKMS recipe for L4T kernels.
- CI: build on x86_64 and aarch64, run `cargo test` (there are no tests yet; the daemon
  needs a fake capturer to test the request loop, the keymap and the batch runner are
  pure and easy).

## oh-my-pi backend

- A `pi-natives` desktop backend that talks to `cuad`, so omp's own `computer` tool
  works on Wayland, lock screen included. About a day.
