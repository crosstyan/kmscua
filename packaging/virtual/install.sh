#!/bin/sh
# Install the kmscua virtual desktop: vkms.ko, udev seat rule, libseat shim,
# weston config and units. Run after packaging/install.sh.
# Usage: packaging/virtual/install.sh [user] [vkms-source-version]
set -eu
here="$(cd "$(dirname "$0")/../.." && pwd)"
user="${1:-${SUDO_USER:-$(id -un)}}"
ver="${2:-}"

# 1. vkms kernel module (skip when the kernel already has it)
if ! /sbin/modinfo vkms >/dev/null 2>&1; then
  [ -f "$here/kernel/vkms/drivers/gpu/drm/vkms/vkms_drv.c" ] || "$here/kernel/vkms/fetch.sh" $ver
  make -C "$here/kernel/vkms"
  sudo make -C "$here/kernel/vkms" install
fi
echo vkms | sudo tee /etc/modules-load.d/kmscua-vkms.conf >/dev/null
sudo /sbin/modprobe vkms

# 2. seat routing for the agent's input devices
sudo install -m644 "$here/packaging/virtual/72-kmscua-virtual.rules" /etc/udev/rules.d/
sudo udevadm control --reload

# 3. libseat noop O_NONBLOCK shim (libseat <= 0.6.x)
sudo mkdir -p /usr/local/lib/kmscua
gcc -shared -fPIC -O2 -o /tmp/libseat-nonblock.so "$here/packaging/virtual/libseat-nonblock.c" -ldl
sudo install -m644 /tmp/libseat-nonblock.so /usr/local/lib/kmscua/libseat-nonblock.so
rm -f /tmp/libseat-nonblock.so

# 4. weston + cuad
sudo mkdir -p /etc/kmscua
sudo install -m644 "$here/packaging/virtual/weston.ini" /etc/kmscua/weston-virtual.ini
sudo install -m755 "$here/packaging/virtual/kmscua-weston.sh" /usr/local/bin/kmscua-weston.sh
sudo install -m644 "$here/packaging/virtual/kmscua-virtual-weston@.service" /etc/systemd/system/
sudo install -m644 "$here/packaging/virtual/cuad-virtual.service" /etc/systemd/system/
sudo mkdir -p /etc/systemd/system/cuad-virtual.service.d
printf "[Service]\nEnvironment=KMSCUA_OWNER=%s\n" "$user" | sudo tee /etc/systemd/system/cuad-virtual.service.d/owner.conf >/dev/null
sudo systemctl daemon-reload
sudo systemctl enable --now "kmscua-virtual-weston@$user.service"
sleep 2
sudo systemctl enable --now cuad-virtual.service
sleep 3
systemctl --no-pager --lines=3 status "kmscua-virtual-weston@$user.service" cuad-virtual.service || true

cat <<MSG

Virtual desktop up. For apps and for cua:
  export WAYLAND_DISPLAY=/run/kmscua-virtual/wayland/wayland-0
  export KMSCUA_SOCKET=/run/kmscua-virtual/cuad/cuad.sock
  cua screenshot -o virt.png

MCP, Claude Code:
  claude mcp add kmscua-virtual -e KMSCUA_SOCKET=/run/kmscua-virtual/cuad/cuad.sock \\
    -e WAYLAND_DISPLAY=/run/kmscua-virtual/wayland/wayland-0 -- /usr/local/bin/cua mcp
MSG
