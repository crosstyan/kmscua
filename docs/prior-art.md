# Prior art and why kmscua looks the way it does

Research done 2026-09-22 on a Jetson Orin running Ubuntu 22.04, GNOME Shell 42.9 on
Wayland. Everything marked *measured* was run on that machine; the rest is linked.

## The problem

An agent needs three things from a Linux desktop: pixels, input, and structure. On
Wayland every one of them is gated by the compositor, and every gate looks different on
GNOME, KDE, wlroots and Xfce. The result is a landscape of tools that each work on one
compositor, on one version, or only after a consent dialog that a background process
cannot click.

## What was tried on this machine (measured)

| Path | Result on GNOME 42 Wayland |
|---|---|
| [`org.gnome.Shell.Screenshot`](https://gitlab.gnome.org/GNOME/gnome-shell/-/work_items/4895) D-Bus | `AccessDenied`. GNOME 41 restricted private D-Bus APIs to an allowlist of well-known bus names (gnome-shell!1970). |
| [`org.freedesktop.portal.Screenshot`](https://flatpak.github.io/xdg-desktop-portal/docs/doc-org.freedesktop.portal.Screenshot.html) | Cancels for a caller without a foreground window. |
| `gnome-screenshot -f` | Works, only because its bus name is on the allowlist. GNOME 49 [removed that exception](https://extensions.gnome.org/extension/9127/allow-gnome-screenshot/). |
| Portal [RemoteDesktop](https://flatpak.github.io/xdg-desktop-portal/docs/doc-org.freedesktop.portal.RemoteDesktop.html) + [ScreenCast](https://flatpak.github.io/xdg-desktop-portal/docs/doc-org.freedesktop.portal.ScreenCast.html) | Works, but re-prompts every process start: `restore_token` for RemoteDesktop needs xdg-desktop-portal 1.18 and [xdg-desktop-portal-gnome 45](https://github.com/GNOME/xdg-desktop-portal-gnome/blob/main/NEWS). ScreenCast-only restore exists since [1.12](https://github.com/flatpak/xdg-desktop-portal/pull/638) / GNOME 42. |
| `org.gnome.Mutter.RemoteDesktop` + `org.gnome.Mutter.ScreenCast` (the private API [gnome-remote-desktop](https://gitlab.gnome.org/GNOME/gnome-remote-desktop) uses) | Works with no dialog from any same-uid process: full 3840x2160 frame via PipeWire, pointer and keyboard injection in the same pixel space. Confirmed on Shell 50 by [deskwright](https://github.com/tristanmuzzu/deskwright). Rejected for kmscua because it is a private API that any release can close, and GNOME-only. |
| [AT-SPI2](https://www.freedesktop.org/wiki/Accessibility/AT-SPI2/) | Works for GTK, Qt, VTE; Flutter exposes a one-node shell; Electron needs `--force-renderer-accessibility`; GL canvases and video expose nothing. `Cache.GetItems` (one round trip for a whole tree) exists on GTK and gnome-shell, not on Flutter. Extents on Wayland are window-relative logical pixels. Used for `get_focused` only. |
| DRM/KMS scanout via [libdrmtap](https://github.com/fxd0h/libdrmtap) + uinput | Works, no compositor involved, works on the lock screen and the GDM greeter. This is the [RustDesk unattended-Wayland](https://github.com/rustdesk/rustdesk/discussions/15417) mechanism. Chosen. |

The last row needed three things upstream did not provide on Tegra: an `aarch64` build, an
out-of-tree `uinput.ko` (the L4T kernel ships without `CONFIG_INPUT_UINPUT`), and a patch
routing the cursor plane through EGL because the cursor buffer lives in nvmap and `mmap`
returns `ENOSYS`. The patch is [`patches/libdrmtap-cursor-egl-fallback.patch`](../patches/libdrmtap-cursor-egl-fallback.patch).

## Wayland primitives, version map

| Primitive | Needs | Dialog |
|---|---|---|
| portal ScreenCast `restore_token` | [xdg-desktop-portal 1.12](https://github.com/flatpak/xdg-desktop-portal/pull/638), xdp-gnome 42 | once |
| portal RemoteDesktop `restore_token` | xdp 1.18, [xdp-gnome 45](https://github.com/GNOME/xdg-desktop-portal-gnome/blob/main/NEWS) | once |
| portal `ConnectToEIS` ([libei](https://gitlab.freedesktop.org/libinput/libei)) | [mutter 45](https://bugs.debian.org/1050241), xdp-gnome 45 | once |
| Mutter private RemoteDesktop / ScreenCast | GNOME 3.26+ | none, private |
| `org.gnome.Shell.Screenshot` | allowlisted names only since 41 | n/a |
| [gnome-remote-desktop headless login](https://release.gnome.org/46/) | GNOME 46 | none, RDP auth |
| KWin [`ScreenShot2` + `EIS.RemoteDesktop`](https://github.com/isac322/kwin-mcp) | Plasma 6 | none |
| [wlr-screencopy](https://www.mankier.com/1/wayvnc) + [virtual pointer/keyboard](https://github.com/niri-wm/niri/pull/4548) | wlroots, niri | none |
| DRM/KMS + uinput | CAP_SYS_ADMIN, `/dev/uinput` | none, any compositor |

## Community projects, ranked by relevance

1. **[agent-sh/computer-use-linux](https://github.com/agent-sh/computer-use-linux)** (Rust,
   MIT; vendored in [codex-desktop-linux](https://github.com/ilysenko/codex-desktop-linux)).
   The most serious Linux implementation: MCP, AT-SPI snapshots, a uinput absolute pointer,
   ydotool, xdotool, a thorough `doctor`. Capture chain is GNOME Shell D-Bus, a shell
   extension, the portal, then `gnome-screenshot`; no PipeWire, no DRM. The extension is
   ESM and declares Shell 45-50, so on 42 it degrades to `gnome-screenshot`. kmscua's
   absolute-pointer design comes from its `abs_pointer.rs`.
2. **[deskwright](https://github.com/tristanmuzzu/deskwright)** (Apache-2.0). GNOME-only
   MCP: AT-SPI, a shell extension for screenshots and windows, the private Mutter
   RemoteDesktop API for input, optional headless `gnome-shell --headless` sessions.
3. **[kwin-mcp](https://github.com/isac322/kwin-mcp)** (MIT). The KDE Plasma 6 equivalent:
   `org.kde.KWin.EIS.RemoteDesktop`, `ScreenShot2`, AT-SPI, isolated `kwin_wayland --virtual`
   sessions.
4. **[RustDesk unattended Wayland](https://rustdesk.com/blog/unattended-remote-access-wayland)**
   (AGPL-3.0, [merged 2026-08](https://github.com/rustdesk/rustdesk/discussions/15417)). Not
   portals, not mutter: the root `--service` reads the KMS scanout via
   [libdrmtap](https://github.com/fxd0h/libdrmtap) (MIT) and ships the dma-buf over a
   unix socket, the user `--server` EGL-detiles on a render node, input is uinput. Works
   at the greeter. kmscua lifts the design, links libdrmtap directly, and rewrites the
   uinput side; nothing from RustDesk's source is copied.
5. **[Cua Driver](https://cua.ai/docs/explanation/linux-and-wayland)**
   ([trycua/cua](https://github.com/trycua/cua)). AT-SPI + XTEST + a painted cursor; native
   Wayland behind a preview flag, mutation refused on GNOME/KDE Wayland except a validated
   Sway config.
6. **[open-codex-computer-use](https://github.com/iFurySt/open-codex-computer-use)**. The
   macOS Swift server is the product; the Linux runtime is a Go shim spawning Python per
   call with GDK root capture, which is black on Wayland.
7. **[oh-my-pi](https://github.com/can1357/oh-my-pi) `computer` tool**. Rust natives: X11 via
   RandR/XTEST, Wayland via the portal RemoteDesktop plus libei, PipeWire capture compiled
   out of release builds. Off by default. A kmscua backend for `pi-natives` would make it
   work on Wayland.
8. **[Grok Bot](https://cursor.com/docs/grok-bot)** (xAI + Cursor). Computer use runs in a
   cloud Debian VM per bot; the local machine only gets approved shell. A UX reference,
   not a backend.
9. **[Bytebot](https://github.com/bytebot-ai/bytebot)**, [OSWorld](https://github.com/xlang-ai/OSWorld),
   [Agent S](https://github.com/simular-ai/agent-s), [UI-TARS desktop](https://github.com/bytedance/UI-TARS-desktop),
   [Open Interpreter](https://github.com/openinterpreter/openinterpreter),
   [Self-Operating Computer](https://github.com/OthersideAI/self-operating-computer),
   [OpenClaw](https://docs.openclaw.ai/nodes/computer-use): all X11 (Xvfb, xdotool or
   pyautogui). Not Wayland.

No vendor ships native Linux GUI control as of 2026-09:
[OpenAI Codex computer use](https://learn.chatgpt.com/docs/computer-use) is macOS/Windows
([Linux issue open](https://github.com/openai/codex/issues/42846)), Anthropic's
[Claude Desktop Linux beta](https://support.claude.com/en/articles/10065433-install-claude-desktop)
excludes computer use, and Cursor's Linux app runs the GUI in the cloud.

## Why only `get_focused` from AT-SPI

In the [OSWorld](https://arxiv.org/html/2404.07972v2) paper (2024) the accessibility
tree more than doubled GPT-4V's success rate over screenshots (12.2% vs 5.3%). By 2026
pixel-only agents score 60-70% on OSWorld-Verified and the leading entries do not read the
tree. What pixels still miss is narrow: verbatim text (paths, numbers, what was just
typed), caret position, and which app owns the active window. That is one read-only
call, ~150-300 ms on this machine, so that is the tool. The tree as an action space is
skipped: Claude is trained on pixels, Codex on the macOS AX grammar, and a Linux AT-SPI
tree is a dialect neither has seen.

## Agent-facing recording: what works elsewhere

Nobody hands a model a video. The patterns that work:

- **[Playwright](https://playwright.dev/docs/videos)** records `.webm` and a
  [trace](https://playwright.dev/docs/trace-viewer) with a screenshot filmstrip plus
  before/after DOM snapshots; [`toHaveScreenshot`](https://playwright.dev/docs/api/class-pageassertions)
  waits until two consecutive screenshots match.
- **[browser-use](https://github.com/browser-use/browser-use/blob/main/browser_use/agent/gif.py)**
  compiles history into a GIF, one frame per step with the goal overlaid.
- **[Cua trajectories](https://cua.ai/docs/agent-sdk/callbacks/trajectories)** save messages,
  calls and screenshots with a viewer.
- **[OSWorld](https://github.com/xlang-ai/OSWorld)** keeps `traj.jsonl`, `step_*.png` and
  `recording.mp4` side by side.
- **codex `record-replay-linux`** ([in codex-desktop-linux](https://github.com/ilysenko/codex-desktop-linux))
  records events and a timeline, not video, and compiles them into a skill.
- **[vncdotool](https://github.com/sibson/vncdotool/pull/477)** has `stable SECONDS [FUZZ]` and
  a region variant `rstable`.
- **[DynamicUI / DynamicGUIBench](https://arxiv.org/html/2604.25380v1)** shows that frames
  between actions are where single-screenshot agents go blind, and that a few relevant
  keyframes beat many; [GUI-World](https://arxiv.org/pdf/2406.10819) shows models fail on
  dynamic content without keyframes or operation history.

kmscua's `record_stop` manifest + timeline + contact sheet, `record_frames … between`,
`wait_for_stable` and `wait_for_change` follow directly from these.

## Image pipeline (measured)

The first version resized with `image::imageops::resize`, which was the whole screenshot
budget. On a real 4K frame, one thread unless noted:

| Step | Time |
|---|---|
| [`image`](https://github.com/image-rs/image) `resize` Triangle 4K→1080p | 134 ms |
| `image` `resize` Nearest | 84 ms |
| hand-written 2x2 box filter | 22 ms, ~5 ms across 12 cores |
| [`fast_image_resize`](https://github.com/Cykooz/fast_image_resize) Box (NEON) | 27 ms, 10.5 ms with `rayon` |
| `fast_image_resize` Bilinear 4K→1568 | 33 ms, 13 ms with `rayon` |
| `image` / [`png`](https://github.com/image-rs/image-png) Fast+Sub, 1080p | 9-11 ms, 1.1 MB |
| `png` Default+Adaptive | 280 ms, 0.8 MB |
| [`zune-png`](https://github.com/etemesi254/zune-image) default | 16 ms, 8 MB (no compression) |
| `image` JPEG q85 | 36 ms + 5 ms RGB conversion |
| [`jpeg-encoder`](https://github.com/vstroebel/jpeg-encoder) q85 from RGBA | 27 ms |

kmscua uses the parallel box filter for the integer part of the reduction and
`fast_image_resize` Bilinear for the residual, then Fast+Sub PNG.

## Vendor tool shapes

Recorded in [vendor-tool-shapes.md](vendor-tool-shapes.md). kmscua's MCP surface copies the
Claude family verbatim; the Codex family is in [TODO.md](../TODO.md).
