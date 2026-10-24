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
u=https://github.com/crosstyan/kmscua/releases/download/xcua-v0.1.1/xcua-x86_64-unknown-linux-musl
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

Then start the app in the background (`app >/tmp/app.log 2>&1 &`). Every command
starts a screen (without a window manager) if none runs, so run `up` first. Resize with `xcua up --size 1280x800 --restart` (apps close).
`--wm auto` starts jwm, openbox, fluxbox, icewm or twm, whichever is installed.
Without a WM, windows have no decorations and a click gives the window the
keyboard. With a WM, focus may follow the pointer (twm's does): do not move the
pointer between clicking a field and typing. `xcua down` stops it all.
The platform may end the screen or a recording with your exec session: if
`xcua status` shows no display in a new session, run `xcua up` again and restart
your apps; if `xcua record status` says not recording, that recording ended.

## Browsers

```sh
xcua up --wm auto                    # Chromium can stall on its first run without a WM
. ~/.local/state/xcua/env
chromium --no-first-run --no-default-browser-check --disable-dev-shm-usage \
  --use-angle=gl --ignore-gpu-blocklist https://example.com >/tmp/chromium.log 2>&1 &
xcua wait-stable --timeout 20000     # the first page takes seconds to draw
```

Add `--no-sandbox` only when you run as root in a container. Without a GPU,
`--use-angle=gl --ignore-gpu-blocklist` renders with Mesa (llvmpipe); if
WebGL still fails, try `--use-angle=swiftshader --enable-unsafe-swiftshader`.

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
| `type 'text'` / `echo text \| xcua type -` | Type into the focused window, any Unicode. `-` reads stdin, minus one trailing newline. |
| `key ctrl+l` / `key ctrl+a BackSpace` / `key Return --repeat 3` | Press one or more key chords in order. |
| `hold shift --seconds 2` | Hold keys down, then release them. |
| `cursor` | Pointer position, in the last screenshot's pixels. |
| `zoom X0 Y0 X1 Y1 -o z.png` | Magnify a region, to read small text. |
| `wait-stable` / `wait-change` `[--timeout 8000] [--region x,y,w,h]` | Wait until the screen stops changing / changes. Exit 3 on timeout is not an error (a caret blinks); `--region` watches one part. |
| `focused` | Which window has the keyboard: title, class, pid. |
| `batch '<json>'` / `batch -` | Several actions in one call (see below). |
| `record start\|mark\|status\|stop\|frames` | Screen recording (see below). |
| `doctor`, `status`, `up`, `down` | Health, state, start and stop the screen. |

## Several actions at once

The MCP `computer_batch` shape; stops at the first failure. Screenshots go to `~/.local/state/xcua/shots/`.

```sh
xcua batch '{"actions": [{"action": "left_click", "coordinate": [640, 360]},
  {"action": "type", "text": "query"}, {"action": "key", "text": "Return"}]}'
```

## Recording what happened

```sh
xcua record start --name login-test      # detached; returns at once
xcua record mark "submitted the form"    # note in the timeline
xcua record stop                         # timeline, manifest, contact sheet path
xcua record frames --count 6 -o /tmp/frames
```

JPEG stills when the screen changes, every xcua action in the timeline; look at the
contact sheet `stop` writes. Max 600 s and 200 MB each; the newest 5 are kept, under
`~/.local/state/xcua/recordings`.

## Troubleshooting

- Start with `xcua doctor`: display, extensions, focus, notes on what is missing.
- No window: source the env file (DISPLAY), read the app's log.
- Typing goes nowhere: click the window, check `xcua focused`; a click misses: new screenshot.
