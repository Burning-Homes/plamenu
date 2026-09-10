#!/usr/bin/env bash
# Requires dnsmasq, iproute2 and systemd-resolved. Run with sudo.
set -euo pipefail
cd -- "$(dirname -- "${BASH_SOURCE[0]}")"
if ((EUID != 0)); then
  echo "Run with sudo: $0" >&2
  exit 1
fi
/usr/bin/dnsmasq --test --conf-file=dnsmasq.conf
systemctl is-active --quiet systemd-resolved
install -m 0644 dnsmasq.conf /etc/plamenu-webxdc-dns.conf
install -m 0644 plamenu-webxdc-dns.service /etc/systemd/system/plamenu-webxdc-dns.service
systemctl daemon-reload
systemctl enable plamenu-webxdc-dns.service
systemctl restart plamenu-webxdc-dns.service
resolvectl flush-caches
resolvectl query wildcard-check.webxdc.plamenu.local wildcard-check.webxdc.plamenu2.local
