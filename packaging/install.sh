#!/bin/sh
# Build and install kmscua: cuad (root daemon) + cua (client/MCP).
# Usage: packaging/install.sh [user-to-add-to-group]
set -eu
here="$(cd "$(dirname "$0")/.." && pwd)"
user="${1:-${SUDO_USER:-$(id -un)}}"

git -C "$here" submodule update --init
cargo build --release --manifest-path "$here/Cargo.toml"

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
