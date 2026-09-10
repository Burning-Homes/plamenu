#!/usr/bin/env bash
set -euo pipefail

root=$(CDPATH='' cd -- "$(dirname -- "$0")/.." && pwd)
label=${1:-dev}
site_root=${2:-"$root/target/docs/site"}

case "$label" in
  *[!A-Za-z0-9._-]*|''|.|..)
    echo "documentation version must contain only letters, digits, dot, underscore, or hyphen" >&2
    exit 2
    ;;
esac

exec python3 "$root/scripts/docs.py" build --label "$label" --output "$site_root/$label"
