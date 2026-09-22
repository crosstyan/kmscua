#!/bin/sh
# Start weston on the vkms card for the kmscua virtual desktop.
# Runs as the desktop's user (group kmscua for the seat's input devices, video
# for the card). libseat's noop backend opens devices directly, no logind, no VT.
set -eu
card=$(basename "$(readlink -f /dev/dri/by-path/platform-vkms-card)")
[ -n "$card" ] || { echo "vkms card not found (modprobe vkms?)" >&2; exit 1; }
export XDG_RUNTIME_DIR=${XDG_RUNTIME_DIR:-/run/kmscua-virtual/wayland}
export LIBSEAT_BACKEND=noop
shim=/usr/local/lib/kmscua/libseat-nonblock.so
[ -f "$shim" ] && export LD_PRELOAD="$shim${LD_PRELOAD:+:$LD_PRELOAD}"
exec weston --backend=drm --renderer=pixman --drm-device="$card" --seat=seat-kmscua \
  --socket=wayland-0 -c /etc/kmscua/weston-virtual.ini "$@"
