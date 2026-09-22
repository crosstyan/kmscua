# kmscua

Computer use for Linux that works below the compositor.

`cuad` runs as root, reads the GPU scanout through [libdrmtap](https://github.com/fxd0h/libdrmtap)
(DRM/KMS, zero-copy, EGL detile on NVIDIA) and injects input through two uinput devices
it creates once at start. `cua` is the unprivileged client: a CLI and an MCP stdio server
that speaks Anthropic's computer-use vocabulary (`screenshot`, `left_click`, `key`, `type`, ...).

Because nothing talks to a compositor, it works the same on GNOME Wayland, KDE, Xfce,
X11, the lock screen and the GDM greeter, with no consent dialog, and it coexists with a
running RustDesk session (both are read-only DRM clients; input devices are independent).

Measured on a Jetson Orin (GNOME 42 Wayland, 3840x2160, nvidia-drm): grab 20-36 ms,
resize+PNG to 1280 px 130-160 ms, cursor composited from the hardware cursor plane.

## Layout

```
crates/proto   wire protocol + blocking client (length-prefixed JSON, binary payload)
crates/cuad    root daemon: capture.rs (libdrmtap + cursor composite), input.rs (uinput),
               keymap.rs (US layout, key names), server.rs (unix socket, one request at a time)
crates/cua     CLI + MCP server; session.rs finds the user's Wayland/X env for clipboard paste
third_party/libdrmtap   vendored MIT library, with the Tegra cursor EGL fallback patch applied
packaging/     systemd unit + install script
```

## Install

```
packaging/install.sh            # builds, installs, creates group kmscua, enables cuad.service
sg kmscua -c 'cua doctor'       # or log in again so the group applies
cua screenshot -o shot.png --max-side 1280
cua click 1920 24 && cua key escape
```

Register the MCP server: `claude mcp add kmscua -- /usr/local/bin/cua mcp`.

## Recording

On by default when a GStreamer H.264 encoder exists. `cuad` picks `nvv4l2h264enc` (Jetson),
`nvh264enc` (desktop NVENC), then `x264enc`/`openh264enc`. The encoder runs as a
`gst-launch-1.0` child fed raw RGBA over a pipe, so nothing from GStreamer or the NVIDIA
stack is linked into the root daemon. Frames are box-downscaled in-process (4K -> 1080p is a
2x2 average), one frame per tick, previous frame repeated if a grab is late, so video time
equals wall time. Files land in `/var/lib/kmscua/recordings`, owned by the socket owner.

```
cua record start --name demo --fps 15
cua record stop -o ~/demo.mp4
cua record sheet ~/demo.mp4 -o sheet.png --tiles 6
cua record frames ~/demo.mp4 --scene 0.05 --max 6 --out-dir frames/
```

Flags: `--no-record`, `--record-codec x264enc`, `--record-dir`. Encoder detection runs in a
background thread at start (the first `gst-inspect` as root rebuilds the registry, 25 s).

## Agent interface (MCP)

The tool surface copies what Claude models were trained on: the flat tool family of
Anthropic's `computer_toolset_20260801` and Claude Code's own computer-use MCP, with the
same names, parameter spellings and return conventions. Actions answer `OK`, `screenshot`
answers with the image only, `cursor_position` answers `X=…, Y=…`, and `computer_batch`
carries the legacy single-tool action enum and stops at the first failure.

| Tool | Params |
|---|---|
| `screenshot` | (optional `settle`) → image only; the image subsequent coordinates refer to |
| `zoom` | `region: [x0, y0, x1, y1]` → magnified image; coordinates still refer to the full screenshot |
| `left_click` `right_click` `middle_click` `double_click` `triple_click` | `coordinate: [x, y]`, `text` = modifiers held during the click |
| `left_click_drag` | `coordinate`, `start_coordinate` (omit = current cursor), `text` |
| `mouse_move` | `coordinate` |
| `left_mouse_down` `left_mouse_up` | none |
| `cursor_position` | none → `X=…, Y=…` |
| `scroll` | `coordinate` (omit = current cursor), `scroll_direction`, `scroll_amount` (ticks), `text` |
| `type` | `text` (ASCII via keyboard, the rest via clipboard) |
| `key` | `text` xdotool chord, `repeat` |
| `hold_key` | `text`, `duration` seconds |
| `wait` | `duration` → screenshot |
| `computer_batch` | `actions: [{action, coordinate, start_coordinate, text, scroll_direction, scroll_amount, duration, repeat, region}]` |

Every mutating action also accepts `settle: true` to append the settled screenshot, which
no vendor tool has. Extras beyond the vendor shape, kept as separate tools so the trained
vocabulary stays intact:

| Tool | What the model gets back |
|---|---|
| `wait_for_stable {region, quiet_ms, timeout_ms, threshold}` | "settled after 0.9s" or "timed out" + screenshot |
| `wait_for_change {region, timeout_ms, threshold}` | "changed after 0.4s" or "timed out, no change" + screenshot |
| `record_start {name, fps, max_side, max_seconds}` | path, size, codec; every later tool call is stamped into a timeline |
| `record_mark {note}` | stamps free text into the timeline |
| `record_stop {sheet, tiles}` | manifest + numbered timeline + contact sheet |
| `record_frames {path, at[], every, scene, between[i,j], max, max_side}` | stills with timestamps |
| `record_status`, `doctor` | text |

Screenshots default to 1920 px on the long side (`cua mcp --max-side N`), Anthropic's
documented cost/accuracy balance; the models accept up to 2576 px.

The Codex family (`list_apps`, `get_app_state {app}` returning a numbered accessibility
tree plus a window screenshot, `click {app, element_index}`, `perform_secondary_action`,
`set_value`, `select_text`, `press_key`, `type_text`, `scroll {pages}`, `drag {from_x…}`)
is app-scoped and accessibility-first. It will be a second profile once the AT-SPI layer
exists; the wire shapes are recorded in the research notes.

## Coordinates

Everything is scanout pixels. The uinput pointer's ABS range equals the union of the
active displays, so `ABS(x, y)` lands on scanout pixel `(x, y)`, the same pixel the
screenshot shows. The MCP layer hides even that: coordinates a model passes are pixels
of the last screenshot it received, and `cua` converts them back.

## Requirements

- Kernel `uinput` (L4T does not ship it; see `~/ThirdParty/rustdesk/uinput-mod` for an
  out-of-tree build) and a DRM card with an active CRTC (a blanked output captures black;
  `cua wake` nudges it).
- Root for `cuad`. The libdrmtap privilege-helper split (`--helper`) is wired but not
  packaged yet.
- `wl-clipboard` or `xclip` in the session for non-ASCII typing (clipboard paste).
- `ffmpeg` on the client side for contact sheets and frame extraction (recording itself
  does not need it).

## Not yet

- AT-SPI tree (`get_app_state`, click by element index). Planned as a `cua` feature that
  talks to the session bus found by `session.rs`.
- Multi-CRTC capture stitched into one image. `displays` lists them; `screenshot` grabs
  the configured CRTC.
- xkb-aware typing for non-US layouts (today: US map for ASCII, clipboard for the rest).
- Packaged helper/setcap mode instead of a root service.
