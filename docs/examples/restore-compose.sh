#!/bin/sh
set -eu

# Run on a replacement host with fresh Compose volumes.
# Restore deploy/ configuration and select the backed-up image first.

# ANCHOR: restore
docker compose --env-file deploy/.env -f deploy/compose.yml up -d --wait db
docker compose --env-file deploy/.env -f deploy/compose.yml exec -T db \
  pg_restore --exit-on-error --no-owner -U plamenu -d plamenu \
  < plamenu-backup-2026-09-09/plamenu.dump
docker compose --env-file deploy/.env -f deploy/compose.yml create plamenu
docker run --rm -i --network none \
  -v plamenu_media-data:/data \
  alpine:3.22 tar -C /data -xzf - \
  < plamenu-backup-2026-09-09/plamenu-media.tgz
docker compose --env-file deploy/.env -f deploy/compose.yml run --rm --no-deps plamenu \
  federation keys audit
docker compose --env-file deploy/.env -f deploy/compose.yml up -d --wait
# ANCHOR_END: restore
