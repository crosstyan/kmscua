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
| scanout grab with cursor | 20-36 ms |
| resize + PNG to 1920 px | 130-160 ms |
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
resized in-process and encoded to PNG or JPEG.

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
| `record_stop {sheet, tiles}` | manifest + numbered timeline + contact sheet |
| `record_frames {path, at[], every, scene, between[i,j], max, max_side}` | stills with timestamps |
| `record_status`, `doctor` | text |

The model never receives a video. It gets a manifest, stills, and a timeline, which is
what Playwright traces, browser-use, Cua and the GUI-agent papers converge on. The
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
cua record frames rec.mp4 --scene 0.05 --max 6 --out-dir frames/
cua mcp [--max-side 1920]
```

Coordinates on the CLI are scanout pixels. `cuad --check` grabs one frame and creates
the input devices without serving; `cuad --no-record`, `--record-codec`, `--record-dir`
control recording.

## Not yet

See [TODO.md](TODO.md). The big one is the Codex profile (app-scoped, accessibility-first,
`get_app_state` with a numbered AT-SPI tree), which is fully specified and waits on the
AT-SPI layer.

## License

MIT. `third_party/libdrmtap` is MIT, copyright Mariano Abad; see `LICENSE.libdrmtap`.
