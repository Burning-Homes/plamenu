#!/usr/bin/env bash
set -euo pipefail

root=$(CDPATH='' cd -- "$(dirname -- "$0")/.." && pwd)
cd "$root"

for tool in cargo mdbook python3; do
  if ! command -v "$tool" >/dev/null 2>&1; then
    echo "documentation check requires $tool" >&2
    exit 1
  fi
done

temporary=$(mktemp -d)
trap 'rm -rf "$temporary"' EXIT

for example in docs/examples/*.sh; do
  bash -n "$example"
done
if command -v shellcheck >/dev/null 2>&1; then
  shellcheck docs/examples/*.sh
fi

cargo build --locked -p plamenu --bin plamenu
python3 scripts/generate-cli-reference.py \
  "${CARGO_TARGET_DIR:-target}/debug/plamenu" "$temporary/cli.md"
diff -u docs/reference/cli.md "$temporary/cli.md"

mdbook test docs
mdbook build docs
python3 scripts/check-built-doc-links.py target/docs/book
