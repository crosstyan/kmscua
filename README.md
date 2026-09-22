# kmscua

Computer use for Linux that works below the compositor.

`cuad` runs as root, reads the GPU scanout through
[libdrmtap](https://github.com/fxd0h/libdrmtap) (DRM/KMS, zero-copy, EGL detile on NVIDIA)
and injects input through two uinput devices it creates once at start. `cua` is the
unprivileged client: a CLI, and an MCP server whose tools copy the computer-use vocabulary
Claude models are trained on.

Because nothing talks to a compositor, it behaves the same on GNOME Wayland, KDE, Xfce,
X11, the lock screen and the GDM greeter. No consent dialog, no portal, no private
compositor API that a future release can close. It coexists with a running RustDesk
session: both are read-only DRM clients, and the input devices are independent.

Measured on a Jetson Orin (GNOME 42 Wayland, 3840x2160, nvidia-drm):

| Operation | Time |
|---|---|
| scanout grab with cursor, BGRX to RGBA | 12-20 ms |
| downscale 4K to 1920 or 1568 px + PNG | 15-22 ms |
| downscale to 1280 px + PNG | 9-11 ms |
| downscale to 2576 px + PNG | 45-50 ms |
| screen recording | 1080p at 15 fps, hardware H.264, zero dropped frames |

Why this route and not the others: [docs/prior-art.md](docs/prior-art.md).

## Install

```
packaging/install.sh                # builds, installs, creates group kmscua, enables cuad.service
cua doctor                          # everything should say true
cua screenshot -o shot.png --max-side 1280
cua click 1920 24 && cua key escape
claude mcp add kmscua -- /usr/local/bin/cua mcp
```

For oh-my-pi, add to `~/.omp/agent/mcp.json`:

```json
{ "mcpServers": { "kmscua": { "type": "stdio", "command": "/usr/local/bin/cua", "args": ["mcp"] } } }
```

Requirements: a DRM card with an active CRTC, kernel `uinput` (L4T ships without it; a
DKMS-style build is in the TODO), root for `cuad`, `wl-clipboard` or `xclip` in the
session for non-ASCII typing, `ffmpeg` for contact sheets and frame extraction, and for
recording a GStreamer H.264 encoder (`nvv4l2h264enc` on Jetson, `nvh264enc`, `x264enc` or
`openh264enc` elsewhere).

## Layout

```
crates/proto   wire protocol + blocking client: u32 length-prefixed JSON, binary payload after
crates/cuad    root daemon
               capture.rs   libdrmtap grab, cursor composite, crop, resize, encode
               input.rs     uinput absolute pointer + keyboard; every request atomic
               keymap.rs    US layout and xdotool key names
               recorder.rs  fixed-rate frame loop into a gst-launch child, MP4 out
               server.rs    unix socket loop + capture worker thread
crates/cua     client
               main.rs      CLI
               mcp.rs       MCP server (rmcp), coordinate mapping, wait/record extras
               video.rs     ffmpeg contact sheets, frames, scene changes, timeline sidecar
               session.rs   finds the user's Wayland/X/D-Bus env from /proc
third_party/libdrmtap   vendored MIT library with the Tegra cursor EGL fallback patch
packaging/     systemd unit + install script
docs/          prior art, vendor tool shapes
```

## How it works

**Capture.** `drmtap_grab_mapped` returns the current scanout as tightly packed BGRX (EGL
detiles block-linear buffers on NVIDIA). The hardware cursor lives on its own KMS plane,
so it is read separately and composited with premultiplied alpha. Frames are cropped and
downscaled in two stages, an integer box filter with rows split across cores for the
bulk of the reduction and [`fast_image_resize`](https://github.com/Cykooz/fast_image_resize)
(NEON, rayon) for the residual so `max_side` is exact, then encoded to PNG (fast zlib
level, Sub filter, about 1 MB for a 1080p desktop) or JPEG. The `image` crate's own
resampler was measured at 134 ms for 4K to 1080p; see the pipeline table in
[docs/prior-art.md](docs/prior-art.md#image-pipeline-measured).

**Input.** The pointer is a uinput device with `ABS_X`/`ABS_Y` whose range equals the
union of the active displays, so `ABS(x, y)` lands on scanout pixel `(x, y)`, the same
pixel the screenshot shows. The keyboard advertises `KEY_ESC..=KEY_MICMUTE` only:
advertising `BTN_*` codes gets the device tagged as a joystick and libinput drops it.
Every request releases whatever it pressed before it answers, including on error, so
another pointer (a human, RustDesk) can interleave between requests but never inside one.

**Recording.** A capture worker thread owns the DRM context. While recording it grabs a
frame every tick, box-downscales by an integer factor (4K to 1080p is a 2x2 average) and
writes raw RGBA to `gst-launch-1.0 fdsrc ! rawvideoparse ! <encoder> ! h264parse ! mp4mux`.
One frame per tick, the previous frame repeated if a grab is late, so video time equals
wall time. Closing the pipe sends EOS and the MP4 trailer is written. GStreamer runs out
of process; nothing from it or from the NVIDIA stack is linked into the root daemon.

**Privilege.** `cuad` needs `CAP_SYS_ADMIN` for `drmModeGetFB2` and rw `/dev/uinput`. It
listens on `/run/kmscua/cuad.sock`, mode 0660, group `kmscua`, chowned to the installing
user via a systemd drop-in so no re-login is needed. Recordings land in
`/var/lib/kmscua/recordings`, owned by that user.

## Agent interface (MCP)

The tool surface copies the flat tool family of Anthropic's `computer_toolset_20260801`
and Claude Code's own computer-use MCP: same names, same parameter spellings, same return
conventions. Actions answer `OK`, `screenshot` answers with the image only,
`cursor_position` answers `X=…, Y=…`, and `computer_batch` runs the legacy action enum
and stops at the first failure. Details and the other vendors' shapes:
[docs/vendor-tool-shapes.md](docs/vendor-tool-shapes.md).

| Tool | Params |
|---|---|
| `screenshot` | → image; the image later coordinates refer to |
| `zoom` | `region: [x0, y0, x1, y1]` → magnified image; coordinates still refer to the full screenshot |
| `left_click` `right_click` `middle_click` `double_click` `triple_click` | `coordinate: [x, y]`, `text` = modifiers held during the click |
| `left_click_drag` | `coordinate`, `start_coordinate` (omit = current cursor), `text` |
| `mouse_move` | `coordinate` |
| `left_mouse_down` `left_mouse_up` | none |
| `cursor_position` | none → `X=…, Y=…` |
| `scroll` | `coordinate` (omit = current cursor), `scroll_direction`, `scroll_amount`, `text` |
| `type` | `text` (ASCII via keyboard, the rest via clipboard) |
| `key` | `text` xdotool chord, `repeat` |
| `hold_key` | `text`, `duration` |
| `wait` | `duration` → screenshot |
| `computer_batch` | `actions: [{action, coordinate, start_coordinate, text, scroll_direction, scroll_amount, duration, repeat, region}]` |

Extras that no vendor ships, kept as separate tools so the trained vocabulary stays
intact. Every mutating action also accepts `settle: true` to append the settled screenshot.

| Tool | Returns |
|---|---|
| `wait_for_stable {region, quiet_ms, timeout_ms, threshold}` | "settled after 0.9s" or "timed out" + screenshot |
| `wait_for_change {region, timeout_ms, threshold}` | "changed after 0.4s" or "timed out, no change" + screenshot |
| `record_start {name, fps, max_side, max_seconds}` | path, size, codec; every later tool call is stamped into a timeline |
| `record_mark {note}` | stamps free text into the timeline |
| `record_stop {sheet, tiles}` | manifest + numbered timeline + contact sheet; writes `<name>.json` beside the MP4 |
| `record_frames {path, at[], every, scene, region, between[i,j], max, max_side, sheet}` | stills with timestamps, or one tiled sheet; with `scene` every frame of `region` is compared and the runs of changed frames are listed |
| `record_status`, `doctor` | text |
| `get_focused` | active app, window title, focused element with its exact text and caret (AT-SPI, read-only) |

The model never receives a video. It gets a manifest, stills, and a timeline, which is
what Playwright traces, browser-use, Cua and the GUI-agent papers converge on. The
manifest (`<recording>.json`) carries the wall-clock start with milliseconds, the
scanout size and scale, fps, codec, frame and drop counts, and the timeline with
wall-clock stamps, so a recording lines up with application logs and screen coordinates
map to video pixels later. `cua record info` prints it. The
timeline is also written beside the MP4 as `<name>.timeline.jsonl`.

Screenshots default to 1920 px on the long side (`cua mcp --max-side N`), Anthropic's
documented cost/accuracy balance; the models accept up to 2576 px.

## CLI

```
cua screenshot [-o f] [--max-side N] [--jpeg] [--no-cursor] [--region x,y,w,h]
cua click X Y [--button right] [--count 2]      cua move X Y
cua drag X1 Y1 X2 Y2                            cua scroll X Y --dy 3
cua key ctrl+shift+t                            cua type "hello"
cua cursor | displays | status | wake | doctor
cua record start [--name n] [--fps 15] [--max-side 1920] [--max-seconds 600]
cua record stop [-o out.mp4] | status
cua record sheet rec.mp4 -o sheet.png --tiles 6
cua record frames rec.mp4 --scene 0.005 --region 880,300,2120,1800 --sheet --out-dir frames/
cua record changes rec.mp4 --region 880,300,2120,1800 --threshold 0.005 [--json]
cua record info rec.mp4
cua mcp [--max-side 1920] [--tools all|core]
```

`record changes` compares every frame of the region with the one before it and prints the
runs of changed frames, so a one-frame flicker is found without extracting anything.
This is the same detector `record_frames … scene` uses in the MCP.

**Context cost.** Claude Code loads MCP tool schemas lazily, so the whole server costs
about one line per tool name until a tool is used. A harness that loads every schema up
front pays about 18k characters for `--tools all`; `--tools core` keeps the 19
vendor-shaped tools (11k) and leaves recording, `wait_for_*` and `get_focused` to the
CLI, which the agent reads with `--help` only when it needs them.

Coordinates on the CLI are scanout pixels. `cuad --check` grabs one frame and creates
the input devices without serving; `cuad --no-record`, `--record-codec`, `--record-dir`
control recording.

## Virtual desktop

An agent can get its own desktop next to yours, captured and driven by the same tool:
`vkms` (the kernel's virtual KMS driver) provides a second DRM card, weston runs on it
with the pixman renderer on its own seat, and a second `cuad` serves it.

```sh
packaging/virtual/install.sh            # vkms.ko, udev seat rule, weston + cuad units
export WAYLAND_DISPLAY=/run/kmscua-virtual/wayland/wayland-0   # apps go here
export KMSCUA_SOCKET=/run/kmscua-virtual/cuad/cuad.sock        # cua and the MCP too
claude mcp add kmscua-virtual -e KMSCUA_SOCKET=$KMSCUA_SOCKET -e WAYLAND_DISPLAY=$WAYLAND_DISPLAY -- /usr/local/bin/cua mcp
```

Input devices are routed by udev seat, so the real session never sees the agent's
keyboard. Design, the two Jetson quirks (5.18 vkms sources on a 5.15 tree, a libseat
`O_NONBLOCK` shim) and limits: [docs/virtual-desktop.md](docs/virtual-desktop.md).

## Limitations

- **Pixels first, almost no window metadata.** kmscua does not know where windows are.
  Input goes to whatever the compositor thinks is focused, screenshots are the whole
  scanout, and there is no per-window crop or app scoping. The one exception is
  `get_focused`, which asks AT-SPI for the active app, the window title and the focused
  element's exact text: enough to verify what was typed without a `zoom`, not an
  element tree. GTK, Qt and VTE answer well, Electron needs its accessibility flag,
  Flutter, GL and video answer with nothing useful. AT-SPI positions on Wayland are
  window-relative and cannot be mapped to the screenshot, so they are not reported.
- **X11 is not the target.** It works there too (the scanout is below X), but X11 already
  has `xdotool`, `xwd`, `wmctrl` and XTEST with window awareness; kmscua brings nothing
  they lack. Use it on X11 only if you want one tool across both.
- **Needs a KMS scanout.** Root or `CAP_SYS_ADMIN`, an active CRTC on a DRM card.
  Headless works through `vkms` (see Virtual desktop); no VM guests whose scanout is
  host-rendered (virgl), no nested compositors, no remote sessions. A blanked output
  captures black; `cua wake` nudges it.
- **One CRTC per screenshot.** Multi-monitor is not stitched yet; the input range already
  spans all displays.
- **Keyboard layout is US.** ASCII is typed through the virtual keyboard; anything else
  goes through the clipboard, which needs the session's Wayland or X socket to be
  discoverable from `/proc` (it is, from an SSH shell, for the same uid).
- **Compositor shortcuts win.** Super, Alt+Tab and friends are handled by the compositor
  before any app sees them, the same as for a physical keyboard.
- **Cursor position is approximate on Tegra.** libdrmtap reports a zero hotspot there,
  so `cursor_position` is off by the cursor's hotspot (6-14 px scanout), and the plane
  lags an injected move by one frame.
- **It is not a sandbox.** Anyone who can connect to the socket controls the seat,
  including the lock screen and the greeter. The group and the socket mode are the
  whole boundary. Do not put the socket in a group that untrusted processes share.
- **Recording has no audio** and depends on GStreamer plus an H.264 element; contact
  sheets and frame extraction depend on `ffmpeg` on the client side.
- **Rotated or HDR outputs are untested.** libdrmtap has tone-mapping and the frame
  comes out in scanout orientation; neither has been exercised here.

## Not yet

See [TODO.md](TODO.md). The big one is the Codex profile (app-scoped, accessibility-first,
`get_app_state` with a numbered AT-SPI tree). It is fully specified and deliberately not
built: Claude models are trained on pixels, Codex on the macOS AX grammar, so a Linux
tree is a reading aid for the one and an unfamiliar dialect for the other. `get_focused`
is the reading aid; the tree waits for a model that is trained on it.

## License

MIT. `third_party/libdrmtap` is MIT, copyright Mariano Abad; see `LICENSE.libdrmtap`.
