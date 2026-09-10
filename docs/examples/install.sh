#!/bin/sh
set -eu

# This file is included in the binary installation guide and checked by CI.

# ANCHOR: host
sudo apt-get update
sudo apt-get install -y --no-install-recommends \
  postgresql caddy ffmpeg ca-certificates curl openssl
sudo adduser --system --group --home /var/lib/plamenu \
  --no-create-home plamenu
sudo -u postgres createuser plamenu
sudo -u postgres createdb --owner=plamenu plamenu
# ANCHOR_END: host

# ANCHOR: release
# Replace 0.6.0 with the release you want to install.
version=0.6.0
case "$(uname -m)" in
  x86_64) arch=amd64 ;;
  aarch64) arch=arm64 ;;
  *) echo 'Supported architectures: AMD64 and ARM64' >&2; exit 1 ;;
esac
archive="plamenu-$version-linux-$arch.tar.gz"
mkdir "plamenu-$version"
cd "plamenu-$version"
curl -fLO "https://codefloe.com/plamenu/plamenu/releases/download/v$version/$archive"
tar -xzf "$archive"

sudo install -m 0755 plamenu /usr/local/bin/plamenu
sudo install -m 0644 plamenu.service /etc/systemd/system/plamenu.service
sudo install -d -m 0750 -o root -g plamenu /etc/plamenu
sudo install -m 0640 -o root -g plamenu \
  plamenu.host.toml.example /etc/plamenu/plamenu.toml
sudo install -m 0644 Caddyfile.host.example /etc/caddy/Caddyfile
# ANCHOR_END: release

# ANCHOR: configure
openssl rand -hex 32
sudoedit /etc/plamenu/plamenu.toml
sudoedit /etc/caddy/Caddyfile
# ANCHOR_END: configure

# ANCHOR: start
# CLI commands initialize media storage before systemd creates StateDirectory.
sudo install -d -m 0750 -o plamenu -g plamenu \
  /var/lib/plamenu /var/lib/plamenu/media
sudo -u plamenu /usr/local/bin/plamenu \
  --config /etc/plamenu/plamenu.toml role list
sudo caddy validate --config /etc/caddy/Caddyfile
sudo systemctl daemon-reload
sudo systemctl enable --now postgresql plamenu caddy
# Debian may already have started Caddy with its packaged default config.
sudo systemctl reload caddy
curl -fsS http://127.0.0.1:8420/health
curl -fsS https://social.example.com/ready
# ANCHOR_END: start

# ANCHOR: first-admin
sudo -u plamenu /usr/local/bin/plamenu \
  --config /etc/plamenu/plamenu.toml account add alice \
  --email alice@example.com --password 'replace-with-a-unique-password'
sudo -u plamenu /usr/local/bin/plamenu \
  --config /etc/plamenu/plamenu.toml account set-role alice Owner
# ANCHOR_END: first-admin
