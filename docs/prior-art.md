# Prior art and why kmscua looks the way it does

Research done 2026-09-22 on a Jetson Orin running Ubuntu 22.04, GNOME Shell 42.9 on
Wayland. Everything marked *measured* was run on that machine; the rest cites sources.

## The problem

An agent needs three things from a Linux desktop: pixels, input, and structure. On
Wayland every one of them is gated by the compositor, and every gate looks different on
GNOME, KDE, wlroots and Xfce. The result is a landscape of tools that each work on one
compositor, on one version, or only after a consent dialog that a background process
cannot click.

## What was tried on this machine (measured)

| Path | Result on GNOME 42 Wayland |
|---|---|
| `org.gnome.Shell.Screenshot` D-Bus | `AccessDenied`. GNOME 41 restricted private D-Bus APIs to an allowlist of well-known names. |
| `org.freedesktop.portal.Screenshot` | Cancels for a caller without a foreground window. |
| `gnome-screenshot -f` | Works, only because its bus name is on the allowlist. GNOME 49 removed that exception. |
| Portal RemoteDesktop + ScreenCast | Works, but re-prompts every process start: `restore_token` for RemoteDesktop needs xdg-desktop-portal 1.18 and xdg-desktop-portal-gnome 45. ScreenCast-only restore exists since 1.12 / GNOME 42. |
| `org.gnome.Mutter.RemoteDesktop` + `org.gnome.Mutter.ScreenCast` (private mutter API, what gnome-remote-desktop uses) | Works with no dialog from any same-uid process. Full 3840x2160 frame via PipeWire, pointer and keyboard injection in the same pixel space. Also confirmed on Shell 50 by deskwright. Rejected for kmscua because it is a private API that can be closed at any release, and GNOME-only. |
| AT-SPI2 | Works for GTK, Qt, Flutter; Electron needs `--force-renderer-accessibility`; GL canvases and video expose nothing. |
| DRM/KMS scanout via libdrmtap + uinput | Works, no compositor involved, works on the lock screen and the GDM greeter. This is the RustDesk "unattended Wayland" mechanism. Chosen. |

The last row needed three things that upstream did not provide on Tegra: an `aarch64`
build, an out-of-tree `uinput.ko` (the L4T kernel ships without `CONFIG_INPUT_UINPUT`), and
a patch routing the cursor plane through EGL because the cursor BO lives in nvmap and
`mmap` returns `ENOSYS`. Those live in `~/ThirdParty/rustdesk/` and `third_party/`.

## Wayland primitives, version map

| Primitive | Needs | Dialog |
|---|---|---|
| portal ScreenCast `restore_token` | xdg-desktop-portal 1.12, xdp-gnome 42 | once |
| portal RemoteDesktop `restore_token` | xdp 1.18, xdp-gnome 45 | once |
| portal `ConnectToEIS` (libei) | mutter 45, xdp-gnome 45 | once |
| Mutter private RemoteDesktop/ScreenCast | GNOME 3.26+ | none, private |
| `org.gnome.Shell.Screenshot` | allowlisted names only since 41 | n/a |
| gnome-remote-desktop headless login | GNOME 46 | none, RDP auth |
| KWin `ScreenShot2` + `EIS.RemoteDesktop` | Plasma 6 | none |
| wlr-screencopy + virtual pointer/keyboard | wlroots, niri | none |
| DRM/KMS + uinput | CAP_SYS_ADMIN, `/dev/uinput` | none, any compositor |

## Community projects, ranked by relevance

1. **agent-sh/computer-use-linux** (Rust, MIT; vendored in `codex-desktop-linux`). The most
   serious Linux implementation: MCP, AT-SPI snapshots, uinput absolute pointer, ydotool,
   xdotool, a thorough `doctor`. Capture chain is GNOME Shell D-Bus, a shell extension,
   the portal, then `gnome-screenshot`; no PipeWire, no DRM. The shell extension is ESM and
   declares Shell 45-50, so on 42 it degrades to `gnome-screenshot`. kmscua's uinput
   absolute-pointer idea comes from its `abs_pointer.rs`.
2. **deskwright** (Apache-2.0). GNOME-only MCP: AT-SPI, a shell extension for screenshots
   and windows, the private Mutter RemoteDesktop API for input, optional headless
   `gnome-shell --headless` sessions.
3. **kwin-mcp** (MIT). The KDE Plasma 6 equivalent: `org.kde.KWin.EIS.RemoteDesktop`,
   `ScreenShot2`, AT-SPI, isolated `kwin_wayland --virtual` sessions.
4. **RustDesk `rustdesk-unattended-wayland`** (AGPL-3.0, merged 2026-08). Not portals, not
   mutter: the root `--service` reads the KMS scanout via libdrmtap (MIT) and ships the
   dma-buf over a unix socket, the user `--server` EGL-detiles on a render node, input is
   uinput. Works at the greeter. kmscua lifts the design, links libdrmtap directly, and
   rewrites the uinput side; nothing from RustDesk's source is copied.
5. **Cua Driver** (trycua). AT-SPI + XTEST + painted cursor; native Wayland behind a
   preview flag, mutation refused on GNOME/KDE Wayland except a validated Sway config.
6. **open-codex-computer-use**. macOS Swift is the product; the Linux runtime is a Go
   shim spawning Python per call with GDK root capture (black on Wayland).
7. **oh-my-pi `computer` tool**. Rust natives: X11 via RandR/XTEST, Wayland via the portal
   RemoteDesktop plus libei, PipeWire capture compiled out of release builds. Off by
   default. A kmscua backend for `pi-natives` would make it work on Wayland.
8. **Grok Bot** (xAI + Cursor). Computer use runs in a cloud Debian VM per bot; the local
   machine only gets approved shell. A UX reference, not a backend.
9. **Bytebot, OSWorld, Agent S, UI-TARS desktop, Open Interpreter, Self-Operating
   Computer, OpenClaw**: all X11 (Xvfb, xdotool or pyautogui). Not Wayland.

No vendor ships native Linux GUI control as of 2026-09: OpenAI Codex computer use is
macOS/Windows, Anthropic's Claude Desktop Linux beta excludes computer use, Cursor's Linux
app runs the GUI in the cloud.

## Agent-facing recording: what works elsewhere

Nobody hands a model a video. The patterns that work:

- **Playwright** records `.webm` and a trace with a screenshot filmstrip plus before/after
  DOM snapshots; `toHaveScreenshot` waits until two consecutive screenshots match.
- **browser-use** compiles history into a GIF, one frame per step with the goal overlaid.
- **Cua** saves trajectories (messages, calls, screenshots) with a viewer.
- **OSWorld** keeps `traj.jsonl`, `step_*.png` and `recording.mp4` side by side.
- **codex `record-replay-linux`** records events and a timeline, not video, and compiles
  them into a skill.
- **vncdotool** has `stable SECONDS [FUZZ]` and a region variant `rstable`.
- **DynamicUI / DynamicGUIBench** (arXiv 2604.25380) show that frames between actions
  are where single-screenshot agents go blind, and that a few relevant keyframes beat
  many.

kmscua's `record_stop` manifest + timeline + contact sheet, `record_frames … between`,
`wait_for_stable` and `wait_for_change` follow directly from these.

## Vendor tool shapes

Recorded in [vendor-tool-shapes.md](vendor-tool-shapes.md). kmscua's MCP surface copies the
Claude family verbatim; the Codex family is a TODO.

## Sources

- Anthropic computer use tool: https://platform.claude.com/docs/en/agents-and-tools/tool-use/computer-use-tool
- Claude Code computer use: https://code.claude.com/docs/en/computer-use
- OpenAI computer use (Codex app): https://learn.chatgpt.com/docs/computer-use
- OpenAI Responses computer tool: https://developers.openai.com/api/docs/guides/tools-computer-use-integration
- RustDesk unattended Wayland: https://rustdesk.com/blog/unattended-remote-access-wayland and https://github.com/rustdesk/rustdesk/discussions/15417
- libdrmtap: https://github.com/fxd0h/libdrmtap
- agent-sh/computer-use-linux: https://github.com/agent-sh/computer-use-linux
- deskwright: https://github.com/tristanmuzzu/deskwright
- kwin-mcp: https://github.com/isac322/kwin-mcp
- Cua Linux: https://cua.ai/docs/explanation/linux-and-wayland
- xdg-desktop-portal ScreenCast / RemoteDesktop: https://flatpak.github.io/xdg-desktop-portal/docs/
- GNOME Shell D-Bus caller restriction (gnome-shell!1970): https://gitlab.gnome.org/GNOME/gnome-shell/-/work_items/4895
- DynamicUI: https://arxiv.org/html/2604.25380v1
- GUI-World: https://arxiv.org/pdf/2406.10819
