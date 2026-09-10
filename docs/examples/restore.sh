#!/bin/sh
set -eu

# Run on a replacement host with an empty database and media directory.
# Restore the original configuration and install the matching binary first.

# ANCHOR: restore
sudo systemctl stop plamenu
# The invoking operator owns and opens the backup; pg_restore runs as plamenu.
# shellcheck disable=SC2024
sudo -u plamenu pg_restore --exit-on-error --no-owner \
  --dbname=plamenu < plamenu-backup-2026-09-09/plamenu.dump
sudo install -d -m 0750 -o plamenu -g plamenu \
  /var/lib/plamenu /var/lib/plamenu/media
sudo tar -C /var/lib/plamenu/media -xzf \
  plamenu-backup-2026-09-09/plamenu-media.tgz
sudo chown -R plamenu:plamenu /var/lib/plamenu/media
sudo -u plamenu /usr/local/bin/plamenu \
  --config /etc/plamenu/plamenu.toml federation keys audit
sudo caddy validate --config /etc/caddy/Caddyfile
sudo systemctl daemon-reload
sudo systemctl enable --now postgresql plamenu caddy
sudo systemctl reload caddy
# ANCHOR_END: restore
