#!/bin/sh
# Build and install kmscua: cuad (root daemon) + cua (client/MCP).
# Usage: packaging/install.sh [user-to-add-to-group]
set -eu
here="$(cd "$(dirname "$0")/.." && pwd)"
user="${1:-${SUDO_USER:-$(id -un)}}"

git -C "$here" submodule update --init

# Jetson (L4T): add the zero-copy recorder (feature cuad/jetson), which needs the Jetson
# Multimedia API headers. KMSCUA_NO_JETSON=1 builds without it.
features=""
mmapi="${JETSON_MMAPI_INCLUDE:-/usr/src/jetson_multimedia_api/include}"
if [ -z "${KMSCUA_NO_JETSON:-}" ] && { [ -e /etc/nv_tegra_release ] ||
    tr '\0' '\n' </proc/device-tree/compatible 2>/dev/null | grep -q '^nvidia,tegra'; }; then
    if [ -e "$mmapi/nvbufsurface.h" ]; then
        features="--features cuad/jetson"
        echo "Jetson detected: building with the zero-copy recorder (cuad/jetson)."
    else
        echo "Jetson detected, but $mmapi/nvbufsurface.h is missing (package" \
            "nvidia-l4t-jetson-multimedia-api); recording will use GStreamer." >&2
    fi
fi
# shellcheck disable=SC2086 # $features is empty or two words
cargo build --release --manifest-path "$here/Cargo.toml" $features

sudo install -m755 "$here/target/release/cuad" /usr/local/bin/cuad
sudo install -m755 "$here/target/release/cua" /usr/local/bin/cua
getent group kmscua >/dev/null || sudo groupadd --system kmscua
sudo usermod -aG kmscua "$user"
sudo install -m644 "$here/packaging/cuad.service" /etc/systemd/system/cuad.service
sudo mkdir -p /etc/systemd/system/cuad.service.d
printf "[Service]\nEnvironment=KMSCUA_OWNER=%s\n" "$user" | sudo tee /etc/systemd/system/cuad.service.d/owner.conf >/dev/null
sudo systemctl daemon-reload
sudo systemctl enable --now cuad.service
sleep 2
sudo systemctl --no-pager --lines=5 status cuad.service || true

cat <<EOF

Installed. '$user' was added to group kmscua; a new login (or 'sg kmscua -c "cua doctor"')
is needed before the socket is reachable from an existing shell.

MCP registration, Claude Code:
  claude mcp add kmscua -- /usr/local/bin/cua mcp
Codex (~/.codex/config.toml):
  [mcp_servers.kmscua]
  command = "/usr/local/bin/cua"
  args = ["mcp"]
EOF
