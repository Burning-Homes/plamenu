#!/bin/sh
set -eu

# This file is included in the Incus guide and checked by CI. It expects a
# prepared plamenu.toml and an existing private network/database.

# ANCHOR: storage
incus storage volume create default plamenu-config \
  initial.uid=10001 initial.gid=10001 initial.mode=0700
incus storage volume file push /root/plamenu.toml \
  default plamenu-config/plamenu.toml \
  --uid 10001 --gid 10001 --mode 0600
incus storage volume create default plamenu-media size=50GiB \
  initial.uid=10001 initial.gid=10001 initial.mode=0750
# ANCHOR_END: storage

# ANCHOR: instance
incus remote add plamenu-registry https://codefloe.com --protocol=oci
incus init \
  "plamenu-registry:plamenu/plamenu@sha256:REPLACE_WITH_RELEASE_DIGEST" \
  plamenu \
  -c boot.autostart=true -c limits.cpu=2 -c limits.memory=2GiB \
  --device "eth0,ipv4.address=10.0.0.42"
incus config set plamenu oci.entrypoint \
  "/bin/sh -c 'until ip route | grep -q ^default; do sleep 1; done; exec plamenu --config /etc/plamenu/plamenu.toml serve'"
incus storage volume attach default plamenu-config \
  plamenu config /etc/plamenu
incus storage volume attach default plamenu-media \
  plamenu media /var/lib/plamenu/media
incus config device add plamenu tmp disk source=tmpfs: path=/tmp size=1GiB \
  initial.uid=10001 initial.gid=10001 initial.mode=1777
incus start plamenu
curl -fsS "http://10.0.0.42:8420/ready"
# ANCHOR_END: instance
