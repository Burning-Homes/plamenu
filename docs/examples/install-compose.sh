#!/bin/sh
set -eu

# This file is included in the Compose guide and checked by CI.

# ANCHOR: prepare
# Set PLAMENU_VERSION to the selected release, without the v prefix.
version=${PLAMENU_VERSION:?Set PLAMENU_VERSION to the selected release first}
git clone --branch "v$version" --depth 1 \
  https://codefloe.com/plamenu/plamenu.git
cd plamenu
cp deploy/.env.example deploy/.env
cp deploy/plamenu.toml.example deploy/plamenu.toml
chmod 600 deploy/.env deploy/plamenu.toml
# ANCHOR_END: prepare

# ANCHOR: start
docker compose --env-file deploy/.env -f deploy/compose.yml config
docker compose --env-file deploy/.env -f deploy/compose.yml up -d
docker compose --env-file deploy/.env -f deploy/compose.yml ps
curl -fsS https://social.example.com/ready
# ANCHOR_END: start

# ANCHOR: first-admin
docker compose --env-file deploy/.env -f deploy/compose.yml exec plamenu \
  plamenu --config /etc/plamenu/plamenu.toml account add alice \
  --email alice@example.com --password 'replace-with-a-unique-password'
docker compose --env-file deploy/.env -f deploy/compose.yml exec plamenu \
  plamenu --config /etc/plamenu/plamenu.toml account set-role alice Owner
# ANCHOR_END: first-admin
