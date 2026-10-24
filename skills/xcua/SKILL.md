---
name: xcua
description: See and operate graphical (X11) apps from the shell in a sandbox with no GPU. xcua runs a virtual screen (Xvfb) and gives one-shot commands for screenshots, clicks, typing, keys, waits and screen recordings. Use it when a task needs a browser, an editor or any GUI window, or proof of what a GUI showed.
---

# xcua: a virtual screen you drive from the shell

`xcua` is one static binary. It starts a private X server (Xvfb), screenshots
it and sends it mouse and keyboard input; it never uses your shell's `$DISPLAY`.
Each command does one thing and exits 0, or non-zero with `xcua: error: …`.
`--json` gives JSON. Run xcua commands one at a time.

## Install (once)

If your operator gave you the binary another way, use that: put it at
`$HOME/bin/xcua` and skip the download.

```sh
mkdir -p "$HOME/bin" && cd "$HOME/bin"
u=https://github.com/crosstyan/kmscua/releases/download/xcua-v0.1.2/xcua-x86_64-unknown-linux-musl
curl -fLO "$u" && curl -fLO "$u.sha256" && sha256sum -c xcua-x86_64-unknown-linux-musl.sha256  # must say OK
mv xcua-x86_64-unknown-linux-musl xcua && chmod +x xcua
"$HOME/bin/xcua" doctor     # checks that Xvfb (the xvfb package) is installed; starts nothing
```

A PATH export does not last between exec sessions: below, `xcua` means `"$HOME/bin/xcua"`.

## Start the screen and launch apps

```sh
xcua up --size 1920x1080 --wm auto   # idempotent; prints `export DISPLAY=:N`
. ~/.local/state/xcua/env            # sets DISPLAY for this shell
```

Then start the app in the background (`app >/tmp/app.log 2>&1 &`) and wait for it:
`xcua wait-window --name <title part> --painted`. Every command starts a screen
(without a WM) if none runs, so run `up` first. Resize: `xcua up --size 1280x800
--restart` (apps close). `--wm auto` starts jwm, openbox, fluxbox, icewm or twm,
whichever is installed (none: a warning, and xcua goes on without). Without a WM,
windows have no decorations and a click gives the window the keyboard. With a WM,
focus may follow the pointer (twm's does): do not move the pointer between
clicking a field and typing. `xcua down` stops it all. The screen or a recording
may end with your exec session: if `xcua status` shows no display, run `xcua up`
again and restart your apps; if `record status` says not recording, it ended.

## Browsers

```sh
xcua up --size 1280x800 --wm auto    # Chromium can stall on its first run without a WM
. ~/.local/state/xcua/env
chromium --no-first-run --no-default-browser-check --disable-dev-shm-usage \
  --window-position=0,0 --window-size=1280,800 \
  --use-angle=gl --ignore-gpu-blocklist https://example.com >/tmp/chromium.log 2>&1 &
xcua wait-window --class chrom --painted --timeout 60000   # the first paint can take 30 s
xcua wait-stable
```

`--window-position`/`--window-size` fill the screen with or without a WM;
`--start-maximized` needs a WM; `xcua maximize --class chrom` works afterwards.
Add `--no-sandbox` only when you run as root in a container. GL problems: see Troubleshooting.

## The loop: screenshot, look, act, wait, screenshot

```sh
xcua screenshot -o /tmp/s.png --max-side 1280   # then LOOK at /tmp/s.png
xcua click 640 360                              # pixels of THAT image
xcua type 'hello world'                         # text starting with - works too
xcua key ctrl+l                                 # xdotool key names
xcua wait-stable                                # until the screen stops changing
xcua screenshot -o /tmp/s.png --max-side 1280   # check the result
```

**COORDINATE RULE.** Every x/y is a pixel of the LAST image `xcua screenshot`
wrote: a 1920x1080 screen at `--max-side 1280` gives a 1280x720 image, whose
middle is `click 640 360`. Take a new screenshot whenever the screen may have
changed. `zoom` images never change the coordinates.

## Commands

| Command | What it does |
|---|---|
| `screenshot -o f.png [--max-side N]` | Capture the screen. Later coordinates refer to this image. |
| `click X Y [--button right\|middle] [--double\|--triple] [--mods ctrl]` | Click. The window under the pointer gets the keyboard. |
| `move X Y` / `drag X1 Y1 X2 Y2` | Move the pointer (hover); drag with the left button. |
| `scroll X Y --dy 3` | Wheel. `--dy 3` scrolls down, `--dy -3` up, `--dx` sideways. |
| `type 'text'` / `echo text \| xcua type -` | Type into the focused window, any Unicode. `-` reads stdin, minus one trailing newline. `--delay-ms 40` for apps that drop fast keys. |
| `key ctrl+l` / `key ctrl+a BackSpace` / `key Return --repeat 3` | Press one or more key chords in order. |
| `hold shift --seconds 2` | Hold keys down, then release them. |
| `cursor` | Pointer position, in the last screenshot's pixels. |
| `zoom X0 Y0 X1 Y1 -o z.png` | Magnify a region, to read small text. |
| `wait-stable` / `wait-change` `[--timeout 8000] [--region x,y,w,h]` | Wait until the screen stops changing / changes. Exit 3 on timeout is not an error (a caret blinks); `--region` watches one part. |
| `focused` | Which window has the keyboard: title, class, pid. |
| `wait-window [--name s] [--class s] [--painted] [--timeout ms]` | Wait until a window is mapped (and drawn into). Exit 3 on timeout. |
| `maximize [--class s]` | Make that window (default: the focused one) fill the screen. |
| `batch '{"actions": [{"action": "left_click", "coordinate": [640, 360]}, {"action": "type", "text": "q"}]}'` | Several actions in one call, the MCP `computer_batch` shape; stops at the first failure. `batch -` reads stdin. |
| `record start\|mark\|status\|stop\|frames` | Screen recording (see below). |
| `doctor`, `status`, `up`, `down` | Health, state, start and stop the screen. |

## Recording what happened

```sh
xcua record start --name login-test      # detached; returns at once
xcua record mark "submitted the form"    # note in the timeline
xcua record stop                         # timeline, manifest, contact sheet path
xcua record frames --count 6 -o /tmp/frames
```

JPEG stills when the screen changes, every action in the timeline; look at the contact
sheet `stop` writes. Max 600 s and 200 MB each; the newest 5 are kept (`~/.local/state/xcua`).

## Troubleshooting

- Start with `xcua doctor`: display, extensions, focus, notes on what is missing.
- No window: source the env file (DISPLAY), read the app's log. jwm's log saying
  `xclock` was not found is harmless.
- Browser screenshots all black or all alike: the browser has no working GL. Try
  `--use-angle=swiftshader --enable-unsafe-swiftshader`, or Mesa's llvmpipe with
  `--use-angle=gl --ignore-gpu-blocklist` and `LIBGL_ALWAYS_SOFTWARE=1
  MESA_LOADER_DRIVER_OVERRIDE=llvmpipe` (plus your platform's library paths).
- Typing goes nowhere: click the window, check `xcua focused`; a click misses: new screenshot.
