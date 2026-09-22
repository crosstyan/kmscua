# Virtual desktop on vkms

An agent desktop that is not your desktop, captured and driven by the same kmscua,
with nothing nested and no compositor API involved: a second DRM card that exists
only in the kernel, a compositor on it, and a second `cuad`.

```
 vkms.ko  ──►  /dev/dri/cardN (connector Virtual-1, 1920x1080, dumb buffers)
                  ▲ master                ▲ read-only GetFB2 + mmap
                  │                       │
   weston --backend=drm --renderer=pixman   cuad --device …/platform-vkms-card
          --seat=seat-kmscua                     --seat seat-kmscua
                  ▲ libinput, ID_SEAT=seat-kmscua        │ creates
                  └──── kmscua@seat-kmscua pointer/keyboard (uinput) ◄┘
```

`install.sh` in `packaging/virtual/` sets it all up; the pieces and why each exists:

| Piece | Why |
|---|---|
| `kernel/vkms/` | The L4T kernel ships without `CONFIG_DRM_VKMS`; the module is built out of tree like `uinput.ko`. NVIDIA's "5.15.148" tree carries 5.18-level DRM (`iosys_map`, shmem wrappers, 6-argument `drm_writeback_connector_init`), so the sources are the v5.18.19 ones, not v5.15.148's. |
| `72-kmscua-virtual.rules` | `cuad --seat seat-kmscua` names its uinput devices `kmscua@seat-kmscua …`; the rule sets `ID_SEAT=seat-kmscua` so weston's libinput takes them and GNOME's ignores them, and opens them to group `kmscua` so weston runs unprivileged. The card itself needs no rule: mutter ships one that ignores vkms, and without `ID_SEAT` the card stays on seat0 where logind and gdm do nothing new with it. |
| `libseat-nonblock.so` | libseat 0.6.4's `noop` backend opens devices without `O_NONBLOCK`; libinput drains each evdev fd with a blocking `read` and weston hangs after "Using Pixman renderer". The shim sets the flag after `libseat_open_device`. Fixed in newer libseat. |
| `kmscua-weston.sh` | Resolves the card from `/dev/dri/by-path/platform-vkms-card` (the number moves), runs weston with the pixman renderer (vkms has no render node; dumb buffers are linear XRGB, which is also the cheapest path for libdrmtap), `LIBSEAT_BACKEND=noop` (no logind session, no VT), on seat `seat-kmscua`. |
| `kmscua-virtual-weston@.service` | Runs as the desktop's user with supplementary groups `kmscua` and `video`. Socket at `/run/kmscua-virtual/wayland/wayland-0`; clients use that absolute path as `WAYLAND_DISPLAY`. |
| `cuad-virtual.service` | Second daemon, own socket at `/run/kmscua-virtual/cuad/cuad.sock`, own recordings dir. Starts after weston and restarts until the CRTC is active. |

Clients need their usual `XDG_RUNTIME_DIR` (wl_shm buffers are created there) plus
`WAYLAND_DISPLAY=/run/kmscua-virtual/wayland/wayland-0`; the MCP server needs
`KMSCUA_SOCKET=/run/kmscua-virtual/cuad/cuad.sock` and the same `WAYLAND_DISPLAY` for
clipboard paste.

## Measured

- Capture 1920x1080 from vkms: 10-28 ms grab, no EGL, no detile.
- Click and type through the seat devices land in weston's focused window; the real
  seat0 session sees neither the devices nor the events.
- A weston-terminal running a command typed by `cua` over the virtual socket,
  screenshot read back through the same socket: works.

## Limits

- **No Xwayland** in NVIDIA's weston build (`nvidia-l4t-weston`), so X11-only apps
  do not run here. Ubuntu's own `weston` (9.0) has it; either can be pointed at by
  `kmscua-weston.sh`.
- **Software rendering.** pixman on the CPU; GL apps get llvmpipe through Mesa only if
  Mesa is installed, and NVIDIA's EGL does not attach to vkms.
- **Cursor** is on vkms's cursor plane, which libdrmtap does not read there yet;
  `cursor_position` reports invisible.
- **Same session bus.** Apps started from the user's shell join the user's D-Bus
  session, so `get_focused` still answers for the GNOME session, not weston.
- **One virtual desktop.** vkms creates one card; a second needs a second instance,
  which the driver does not do without configfs (6.x).

## Why vkms and not a nested compositor

Nested compositors (`gnome-shell --headless`, `kwin_wayland --virtual`,
`weston --backend=headless`) render to memory and hand out pixels and input through
their own protocols. That works and is what deskwright and kwin-mcp use, but it is a
different tool per compositor. vkms puts a real KMS scanout in front of the same
capture path, so one `cuad` serves the physical screen and the virtual one alike,
including lock screens and greeters, and any compositor with a DRM backend can sit
on it.
