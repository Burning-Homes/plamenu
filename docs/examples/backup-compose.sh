#!/bin/sh
set -eu

# ANCHOR: backup
# Run from the deployment checkout; use a new directory for each backup.
mkdir -m 700 plamenu-backup-2026-09-09

docker compose --env-file deploy/.env -f deploy/compose.yml stop plamenu
docker compose --env-file deploy/.env -f deploy/compose.yml exec -T db \
  pg_dump -Fc -U plamenu plamenu > plamenu-backup-2026-09-09/plamenu.dump
docker run --rm \
  -v plamenu_media-data:/data:ro \
  -v "$PWD/plamenu-backup-2026-09-09":/backup \
  alpine:3.22 tar -C /data -czf /backup/plamenu-media.tgz .
grep '^PLAMENU_IMAGE=' deploy/.env > plamenu-backup-2026-09-09/image-reference.txt
cp deploy/compose.yml deploy/Caddyfile plamenu-backup-2026-09-09/
sudo chown "$USER" plamenu-backup-2026-09-09/*
docker compose --env-file deploy/.env -f deploy/compose.yml start plamenu
# ANCHOR_END: backup
