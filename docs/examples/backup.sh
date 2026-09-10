#!/bin/sh
set -eu

# This file is both included in the operator guide and checked by CI.

# ANCHOR: backup
# Use a new directory for each backup.
mkdir -m 700 plamenu-backup-2026-09-09

sudo systemctl stop plamenu
# The invoking operator owns the backup directory and opens the output file.
# shellcheck disable=SC2024
sudo -u plamenu pg_dump -Fc plamenu > plamenu-backup-2026-09-09/plamenu.dump
sudo tar -C /var/lib/plamenu/media -czf \
  plamenu-backup-2026-09-09/plamenu-media.tgz .
sudo cp /etc/systemd/system/plamenu.service /etc/caddy/Caddyfile plamenu-backup-2026-09-09/
/usr/local/bin/plamenu --version > plamenu-backup-2026-09-09/plamenu-version.txt
sudo chown "$USER" plamenu-backup-2026-09-09/*
sudo systemctl start plamenu
# ANCHOR_END: backup
